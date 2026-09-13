// Mainnet's own bank hash for a slot, taken from the votes that landed after it.
use slate_replay::block;
use std::collections::HashMap;
fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let rpc = a.next().unwrap();
    let want: Vec<u64> = a.map(|s| s.parse().unwrap()).collect();
    let mut seen: HashMap<u64, HashMap<String, usize>> = HashMap::new();
    let lo = *want.iter().min().unwrap();
    for slot in lo..lo + 60 {
        let Ok(b) = block::fetch_block(&rpc, slot) else {
            continue;
        };
        for (s, h) in block::vote_confirmations(&b) {
            if want.contains(&s) {
                *seen.entry(s).or_default().entry(h.to_string()).or_default() += 1;
            }
        }
    }
    for s in &want {
        match seen.get(s) {
            Some(m) => {
                let mut v: Vec<_> = m.iter().collect();
                v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
                for (h, n) in v {
                    println!("  slot {s}  votes {n:>4}  hash {h}");
                }
            }
            None => println!("  slot {s}  no votes found"),
        }
    }
    Ok(())
}
