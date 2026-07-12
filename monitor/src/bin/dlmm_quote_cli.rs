//! Tiny CLI for the S9 parity harness: quote a Meteora DLMM exact-in swap
//! from RAW captured account files, printing only the integer output.
//!
//! Usage:
//!   dlmm_quote_cli <lbpair.bin> <swap_for_y:0|1> <amount_in> <now_unix> \
//!                  <binarray.bin> [<binarray.bin> ...]
//!
//! Exit code 0 with the quote on stdout, or a structured error on stderr and
//! exit 1. This exercises the EXACT library code (`dlmm_quote_exact_in`) the
//! bot uses — no reimplementation.

use arb_monitor::meteora_dlmm::{decode_bin_array, decode_lb_pair, dlmm_quote_exact_in};
use std::collections::HashMap;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 5 {
        eprintln!("usage: dlmm_quote_cli <lbpair.bin> <swap_for_y:0|1> <amount_in> <now_unix> <binarray.bin>...");
        std::process::exit(2);
    }
    let pair_bytes = std::fs::read(&args[0]).expect("read lbpair");
    let pair = match decode_lb_pair(&pair_bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("decode_lb_pair: {e:?}");
            std::process::exit(1);
        }
    };
    let swap_for_y = args[1] == "1";
    let amount_in: u64 = args[2].parse().expect("amount_in");
    let now_unix: i64 = args[3].parse().expect("now_unix");
    let mut arrays = HashMap::new();
    for path in &args[4..] {
        let bytes = std::fs::read(path).expect("read binarray");
        match decode_bin_array(&bytes) {
            Ok(a) => {
                arrays.insert(a.index, a);
            }
            Err(e) => {
                eprintln!("decode_bin_array {path}: {e:?}");
                std::process::exit(1);
            }
        }
    }
    match dlmm_quote_exact_in(&pair, &arrays, swap_for_y, amount_in, now_unix) {
        Ok(out) => println!("{out}"),
        Err(e) => {
            eprintln!("quote error: {e:?}");
            std::process::exit(1);
        }
    }
}
