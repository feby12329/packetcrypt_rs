// SPDX-License-Identifier: (LGPL-2.1-only OR LGPL-3.0-only)
use bytes::BufMut;
use log::debug;
use packetcrypt_sys::*;
use rayon::prelude::*;
use std::sync::Arc;
use crate::blkminer::BlkMiner;

#[derive(Default, Clone, Copy)]
pub struct AnnData {
    pub hash_pfx: u64,
    pub mloc: u32,
    pub index: u32,
}

pub struct ProofTree {
    raw: *mut ProofTree_t,
    capacity: u32,
    size: u32,
    bm: Arc<BlkMiner>,
    pub root_hash: Option<[u8; 32]>,
    pub ann_data: Vec<AnnData>,
    pub index_table: Vec<u32>,
}

unsafe impl Send for ProofTree {}
unsafe impl Sync for ProofTree {}

impl Drop for ProofTree {
    fn drop(&mut self) {
        unsafe {
            ProofTree_destroy(self.raw);
        }
    }
}

static FFF_ENTRY: ProofTree_Entry_t = ProofTree_Entry_t {
    hash: [0xff_u8; 32],
    start: 0xffffffffffffffff,
    end: 0xffffffffffffffff,
};
fn fff_entry() -> *const ProofTree_Entry_t {
    &FFF_ENTRY as *const ProofTree_Entry_t
}

impl ProofTree {
    pub fn new(max_anns: u32, bm: Arc<BlkMiner>) -> ProofTree {
        ProofTree {
            raw: unsafe { ProofTree_create(max_anns) },
            size: 0,
            capacity: max_anns,
            root_hash: None,
            ann_data: vec![AnnData::default(); max_anns as usize], // TODO: this is going to take ages
            index_table: Vec::with_capacity(max_anns as usize),
            bm,
        }
    }

    pub fn reset(&mut self) {
        self.size = 0;
        self.root_hash = None;
    }

    pub fn compute(&mut self, count: usize) -> Result<(), &'static str> {
        if self.root_hash.is_some() {
            return Err("tree is in computed state, call reset() first");
        }
        let data = &mut self.ann_data[..count];
        if data.is_empty() {
            return Err("no anns, cannot compute tree");
        }

        if data.len() > self.capacity as usize {
            return Err("too many anns");
        }

        // Sort the data items
        data.par_sort_by(|a, b| a.hash_pfx.cmp(&b.hash_pfx));

        self.index_table.clear();
        let mut last_pfx = 0;
        for d in data.iter_mut() {
            // Deduplicate and insert in the index table
            #[allow(clippy::comparison_chain)]
            if d.hash_pfx > last_pfx {
                self.index_table.push(d.mloc);
                // careful to skip entry 0 which is the 0-entry
                d.index = self.index_table.len() as u32;
                last_pfx = d.hash_pfx;
            } else if d.hash_pfx == last_pfx {
                //debug!("Drop ann with index {:#x}", pfx);
                d.index = 0;
            } else {
                panic!("list not sorted {:#x} < {:#x}", d.hash_pfx, last_pfx);
            }
        }
        debug!("Loaded {} out of {} anns", self.index_table.len(), data.len());

        // Copy the data to the location
        self.ann_data[..count].par_iter().for_each(|d| {
            if d.index == 0 {
                // Removed in dedupe stage
                return;
            }
            let e = ProofTree_Entry_t {
                hash: *self.bm.get_hash(d.index as usize).bytes(),
                start: d.hash_pfx,
                end: 0,
            };
            unsafe { ProofTree_putEntry(self.raw, d.index, &e as *const ProofTree_Entry_t) };
        });

        let total_anns_zero_included = self.index_table.len() + 1;
        unsafe { ProofTree_prepare2(self.raw, total_anns_zero_included as u64) };

        // Build the merkle tree
        let mut count_this_layer = total_anns_zero_included;
        let mut odx = count_this_layer;
        let mut idx = 0;
        while count_this_layer > 1 {
            if (count_this_layer & 1) != 0 {
                unsafe { ProofTree_putEntry(self.raw, odx as u32, fff_entry()) };
                count_this_layer += 1;
                odx += 1;
            }
            (0..count_this_layer)
                .into_par_iter()
                .step_by(2)
                .for_each(|i| unsafe {
                    ProofTree_hashPair(self.raw, (odx + i / 2) as u64, (idx + i) as u64);
                });
            idx += count_this_layer;
            count_this_layer /= 2;
            odx += count_this_layer;
        }
        assert!(idx + 1 == odx);
        let mut rh = [0u8; 32];
        assert!(odx as u64 == unsafe { ProofTree_complete(self.raw, rh.as_mut_ptr()) });

        self.root_hash = Some(rh);
        self.size = self.index_table.len() as u32;
        Ok(())
    }

    pub fn get_commit(&self, ann_min_work: u32) -> Result<bytes::BytesMut, &'static str> {
        let hash = if let Some(h) = self.root_hash.as_ref() {
            h
        } else {
            return Err("Not in computed state, call compute() first");
        };
        let mut out = bytes::BytesMut::with_capacity(44);
        out.put(&[0x09, 0xf9, 0x11, 0x02][..]);
        out.put_u32_le(ann_min_work);
        out.put(&hash[..]);
        out.put_u64_le(self.size as u64);
        Ok(out)
    }

    pub fn mk_proof(&mut self, ann_nums: &[u64; 4]) -> Result<bytes::BytesMut, &'static str> {
        if self.root_hash.is_none() {
            return Err("Not in computed state, call compute() first");
        }
        for n in ann_nums {
            if *n >= self.size as u64 {
                return Err("Ann number out of range");
            }
        }
        Ok(unsafe {
            let proof = ProofTree_mkProof(self.raw, ann_nums.as_ptr());
            let mut out = bytes::BytesMut::with_capacity((*proof).size as usize);
            let sl = std::slice::from_raw_parts((*proof).data, (*proof).size as usize);
            out.put(sl);
            ProofTree_destroyProof(proof);
            out
        })
    }
}
