//! Resolves an Anvil palette entry to the baked table's global blockstate id.
//!
//! zvcr stores blocks as ids into `Block.BLOCK_STATE_REGISTRY`, so its importer
//! can index `blockprops.bin` directly. Anvil stores `{Name, Properties}`
//! instead, and Minecraft writes a compound's keys in `HashMap` order, so the
//! properties arrive in no particular order and cannot be compared as text as
//! they stand. Both sides are therefore reduced to the same canonical string —
//! block name plus `key=value` tokens sorted by key — which is unique per
//! state and independent of how either side happened to order them.
//!
//! Keying on the name rather than on a numeric id is also what makes the table
//! usable across versions: the download is 1.21.1 and the baked table is
//! 1.21.4, whose registry order differs, but a state's name and properties do
//! not.

use std::collections::HashMap;

use smallvec::SmallVec;
use xaero_zvcr::blockprops::BlockProps;

/// Properties one blockstate can carry; the widest vanilla blocks (walls,
/// chorus plant) have seven.
pub type Props<'a> = SmallVec<[(&'a str, &'a str); 8]>;

pub struct StateIndex {
    by_key: HashMap<Box<str>, u16>,
    air: u16,
}

impl StateIndex {
    /// Indexes every state in the baked table by its canonical name.
    pub fn build(props: &BlockProps) -> Result<StateIndex, String> {
        if props.len() > u16::MAX as usize {
            return Err(format!(
                "block table has {} states, more than a u16 palette can hold",
                props.len()
            ));
        }
        let mut by_key: HashMap<Box<str>, u16> = HashMap::with_capacity(props.len() * 2);
        let mut key = String::with_capacity(128);
        let mut pairs: Props = SmallVec::new();
        for id in 0..props.len() as u32 {
            let name = props.name(id);
            let (block, rest) = match name.split_once('[') {
                Some((b, r)) => (b, r.strip_suffix(']').unwrap_or(r)),
                None => (name, ""),
            };
            pairs.clear();
            if !rest.is_empty() {
                for token in rest.split(',') {
                    let (k, v) = token
                        .split_once('=')
                        .ok_or_else(|| format!("state {id} has property token {token:?}"))?;
                    pairs.push((k, v));
                }
            }
            canonical(&mut key, block, &mut pairs);
            if let Some(prev) = by_key.insert(key.as_str().into(), id as u16) {
                return Err(format!(
                    "states {prev} and {id} share the canonical name {key:?}"
                ));
            }
        }
        Ok(StateIndex {
            by_key,
            air: props.air() as u16,
        })
    }

    /// Id of `minecraft:air`, which is what an unresolvable state degrades to.
    #[inline]
    pub fn air(&self) -> u16 {
        self.air
    }

    #[inline]
    pub fn get(&self, key: &str) -> Option<u16> {
        self.by_key.get(key).copied()
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

/// Writes `block[k=v,k=v]` into `out` with the properties sorted by key.
/// `pairs` is sorted in place; the caller's scratch buffers are reused across
/// millions of palette entries, so neither allocates here.
pub fn canonical(out: &mut String, block: &str, pairs: &mut Props<'_>) {
    out.clear();
    out.push_str(block);
    if pairs.is_empty() {
        return;
    }
    pairs.sort_unstable_by(|a, b| a.0.cmp(b.0));
    out.push('[');
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out.push(']');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> BlockProps {
        BlockProps::parse(xaero_zvcr::BLOCKPROPS).expect("baked block table")
    }

    #[test]
    fn every_baked_state_is_indexed_exactly_once() {
        let props = table();
        let idx = StateIndex::build(&props).expect("index");
        assert_eq!(idx.len(), props.len());
    }

    #[test]
    fn property_order_does_not_matter() {
        let props = table();
        let idx = StateIndex::build(&props).expect("index");
        let mut key = String::new();

        let mut pairs: Props = SmallVec::new();
        canonical(&mut key, "minecraft:netherrack", &mut pairs);
        let netherrack = idx.get(&key).expect("netherrack");
        assert_eq!(props.name(netherrack as u32), "minecraft:netherrack");

        // Same state, properties handed over in two different orders.
        let mut a: Props = SmallVec::from_slice(&[("facing", "north"), ("lit", "true")]);
        canonical(&mut key, "minecraft:redstone_wall_torch", &mut a);
        let one = idx.get(&key).expect("redstone wall torch");
        let mut b: Props = SmallVec::from_slice(&[("lit", "true"), ("facing", "north")]);
        canonical(&mut key, "minecraft:redstone_wall_torch", &mut b);
        assert_eq!(idx.get(&key), Some(one));
        assert_eq!(
            props.name(one as u32),
            "minecraft:redstone_wall_torch[facing=north,lit=true]"
        );
    }

    #[test]
    fn a_121_1_state_resolves_against_the_121_4_table() {
        // Every state the download can hold exists in the newer table; these
        // are the shapes the walk actually asks about.
        let props = table();
        let idx = StateIndex::build(&props).expect("index");
        let mut key = String::new();
        for (block, pairs) in [
            ("minecraft:water", vec![("level", "0")]),
            ("minecraft:grass_block", vec![("snowy", "false")]),
            (
                "minecraft:oak_leaves",
                vec![
                    ("distance", "7"),
                    ("persistent", "false"),
                    ("waterlogged", "false"),
                ],
            ),
            (
                "minecraft:cobblestone_wall",
                vec![
                    ("east", "none"),
                    ("north", "none"),
                    ("south", "none"),
                    ("up", "true"),
                    ("waterlogged", "false"),
                    ("west", "none"),
                ],
            ),
        ] {
            let mut p: Props = pairs.into_iter().collect();
            canonical(&mut key, block, &mut p);
            assert!(idx.get(&key).is_some(), "{key} did not resolve");
        }
    }
}
