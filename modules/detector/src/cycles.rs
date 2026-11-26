//! Enumeration of candidate cycles through recently changed pools.
//!
//! Every live pool is its own edge, so two pools of the same pair form a
//! two-hop cycle. Keeping only the best rate per pair would miss exactly
//! those arbs, the most common kind when a chain has several venues for its
//! main pairs. Kairos tracks a small, curated pool set, so enumerating every
//! simple cycle of up to `max_hops` through the changed pools is cheap and
//! exhaustive.

use std::collections::{HashMap, HashSet};

use alloy::primitives::Address;
use kairos_dex::Pool;
use kairos_state::StateSnapshot;

/// One hop of a cycle: swap `token_in` for `token_out` in `pool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Hop {
    pub pool: Address,
    pub token_in: Address,
    pub token_out: Address,
}

/// Live pools indexed by token: `token -> [(pool, other token)]`.
pub(crate) struct Adjacency(HashMap<Address, Vec<(Address, Address)>>);

impl Adjacency {
    pub(crate) fn new(snapshot: &StateSnapshot) -> Self {
        let mut map: HashMap<Address, Vec<(Address, Address)>> = HashMap::new();
        for pool in snapshot.registry().iter() {
            let address = pool.address();
            if snapshot.is_syncing(&address) {
                continue;
            }
            let tokens = pool.tokens();
            for &a in tokens {
                for &b in tokens {
                    if a != b {
                        map.entry(a).or_default().push((address, b));
                    }
                }
            }
        }
        for edges in map.values_mut() {
            edges.sort();
        }
        Self(map)
    }

    fn edges(&self, token: Address) -> &[(Address, Address)] {
        self.0.get(&token).map_or(&[], Vec::as_slice)
    }
}

/// Every simple cycle of 2 to `max_hops` hops that uses at least one pool in
/// `changed` and passes through a token in `start_tokens`. Each cycle is
/// rotated to begin at its highest-priority start token (earliest in
/// `start_tokens`) and appears once per direction.
pub(crate) fn cycles_through(
    adjacency: &Adjacency,
    snapshot: &StateSnapshot,
    changed: &[Address],
    start_tokens: &[Address],
    max_hops: usize,
) -> Vec<Vec<Hop>> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for &pool_address in changed {
        let Some(pool) = snapshot.pool(&pool_address) else {
            continue;
        };
        let tokens = pool.tokens();
        for &a in tokens {
            for &b in tokens {
                if a == b {
                    continue;
                }
                let first = Hop {
                    pool: pool_address,
                    token_in: a,
                    token_out: b,
                };
                let mut path = vec![first];
                let mut visited = vec![a, b];
                extend(adjacency, &mut path, &mut visited, max_hops, &mut |cycle| {
                    if let Some(rotated) = rotate(cycle, start_tokens)
                        && seen.insert(rotated.clone())
                    {
                        out.push(rotated);
                    }
                });
            }
        }
    }
    out
}

/// Depth-first extension of `path` back to its first token.
fn extend(
    adjacency: &Adjacency,
    path: &mut Vec<Hop>,
    visited: &mut Vec<Address>,
    max_hops: usize,
    emit: &mut impl FnMut(&[Hop]),
) {
    let origin = path[0].token_in;
    let here = path.last().expect("path is never empty").token_out;
    for &(pool, next) in adjacency.edges(here) {
        if path.iter().any(|h| h.pool == pool) {
            continue;
        }
        let hop = Hop {
            pool,
            token_in: here,
            token_out: next,
        };
        if next == origin {
            path.push(hop);
            emit(path);
            path.pop();
        } else if path.len() + 1 < max_hops && !visited.contains(&next) {
            path.push(hop);
            visited.push(next);
            extend(adjacency, path, visited, max_hops, emit);
            visited.pop();
            path.pop();
        }
    }
}

fn rotate(cycle: &[Hop], start_tokens: &[Address]) -> Option<Vec<Hop>> {
    let start = start_tokens
        .iter()
        .find_map(|t| cycle.iter().position(|h| h.token_in == *t))?;
    Some(
        cycle[start..]
            .iter()
            .chain(&cycle[..start])
            .copied()
            .collect(),
    )
}
