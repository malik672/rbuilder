use super::{BlockBuildingContext, BlockState, PartialBlockFork};
use crate::primitives::{Order, OrderId};
use itertools::Itertools;
use reth::{primitives::Address, providers::StateProviderBox};
use reth_provider::StateProvider;
use revm_primitives::U256;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// Conflict generated by executing an order before another.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Conflict {
    NoConflict,
    /// First order changed a nonce used by the second one.
    Nonce(Address),
    /// First order caused second one to fail.
    Fatal,
    /// Second order executed ok but with different profit.
    DifferentProfit {
        profit_alone: U256,
        profit_with_conflict: U256,
    },
}

pub fn find_conflict_slow(
    state_provider: StateProviderBox,
    ctx: &BlockBuildingContext,
    orders: &[Order],
) -> eyre::Result<HashMap<(OrderId, OrderId), Conflict>> {
    let mut state_provider = Arc::<dyn StateProvider>::from(state_provider);
    let profits_alone = {
        let mut profits_alone = HashMap::with_capacity(orders.len());
        for order in orders {
            let mut state = BlockState::new_arc(state_provider);
            let mut fork = PartialBlockFork::new(&mut state);
            if let Ok(res) = fork.commit_order(order, ctx, 0, 0, 0, true)? {
                profits_alone.insert(order.id(), res.coinbase_profit);
            };
            state_provider = state.into_provider();
        }
        profits_alone
    };

    let mut results = HashMap::new();

    for (order1, order2) in orders.iter().cartesian_product(orders.iter()) {
        if !profits_alone.contains_key(&order1.id()) || !profits_alone.contains_key(&order2.id()) {
            continue;
        }

        if order1.id() == order2.id() {
            continue;
        }

        let pair = (order1.id(), order2.id());
        let mut nonce_map = HashMap::new();
        order1.nonces().into_iter().for_each(|nonce| {
            nonce_map.insert(nonce.address, nonce);
        });
        if let Some(nonce) = order2.nonces().into_iter().find(|nonce| {
            if let Some(nonce_map) = nonce_map.get(&nonce.address) {
                let optional = nonce.optional || nonce_map.optional;
                !optional && nonce.address == nonce_map.address
            } else {
                false
            }
        }) {
            results.insert(pair, Conflict::Nonce(nonce.address));
            continue;
        }

        let mut state = BlockState::new_arc(state_provider);
        let mut fork = PartialBlockFork::new(&mut state);
        let mut gas_used = 0;
        let mut blob_gas_used = 0;
        match fork.commit_order(order1, ctx, gas_used, 0, blob_gas_used, true)? {
            Ok(res) => {
                gas_used += res.gas_used;
                blob_gas_used += res.blob_gas_used;
            }
            Err(_) => {
                results.insert(pair, Conflict::Fatal);
            }
        };
        match fork.commit_order(order2, ctx, gas_used, 0, blob_gas_used, true)? {
            Ok(re) => {
                let profit_alone = *profits_alone.get(&order2.id()).unwrap();
                let profit_with_conflict = re.coinbase_profit;
                let conflict = if profit_alone == profit_with_conflict {
                    Conflict::NoConflict
                } else {
                    Conflict::DifferentProfit {
                        profit_alone,
                        profit_with_conflict,
                    }
                };
                results.insert(pair, conflict);
            }
            Err(_) => {
                results.insert(pair, Conflict::Fatal);
            }
        };
        state_provider = state.into_provider();
    }

    Ok(results)
}

pub fn get_conflict_sets(
    conflicts: &HashMap<(OrderId, OrderId), Conflict>,
) -> Vec<HashSet<OrderId>> {
    let mut set_id = 0;
    let mut conflict_sets = HashMap::<i32, HashSet<OrderId>>::new();
    let mut order_to_conflict_set = HashMap::<OrderId, i32>::new();

    for ((k1, k2), conflict) in conflicts {
        if matches!(conflict, Conflict::NoConflict) {
            continue;
        }

        let set1id = order_to_conflict_set.get(k1).copied();
        let set2id = order_to_conflict_set.get(k2).copied();
        match (conflict, set1id, set2id) {
            (Conflict::NoConflict, _, _) => continue,
            (_, Some(set1id), Some(set2id)) if set1id == set2id => continue,
            (_, Some(set1id), Some(set2id)) => {
                // mesge two conflic sets
                let mut set1 = conflict_sets.remove(&set1id).unwrap();
                let set2 = conflict_sets.remove(&set2id).unwrap();
                for k in set2 {
                    set1.insert(k);
                    order_to_conflict_set.insert(k, set1id);
                }
                conflict_sets.insert(set1id, set1);
            }
            (_, Some(set_id), None) | (_, None, Some(set_id)) => {
                let set = conflict_sets.get_mut(&set_id).unwrap();
                set.insert(*k1);
                set.insert(*k2);
                order_to_conflict_set.insert(*k1, set_id);
                order_to_conflict_set.insert(*k2, set_id);
            }
            (_, None, None) => {
                let mut set = HashSet::new();
                set.insert(*k1);
                set.insert(*k2);
                order_to_conflict_set.insert(*k1, set_id);
                order_to_conflict_set.insert(*k2, set_id);
                conflict_sets.insert(set_id, set);
                set_id += 1;
            }
        }
    }
    let mut conflict_sets = conflict_sets.into_values().collect::<Vec<_>>();
    conflict_sets.sort_by_key(|set| std::cmp::Reverse(set.len()));
    conflict_sets
}
