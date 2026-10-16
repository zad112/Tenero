//! The node's threaded check of one attempt against the one-thread form (`matmulhash::compute_attempt_at_threads`).

/// The node checks a block's attempt on several threads (`compute_attempt_at_threads`); it must be the attempt the one-thread
/// form gives (which the golden vectors pin), bit for bit, in both designs and for any number of threads, including more
/// threads than columns and a number that does not divide them.
#[test]
fn an_attempt_on_several_threads_is_the_same_as_on_one() {
    use tenero_core::matmulhash::{self as mh, Dataset, Params};
    let p = Params {
        m: 64,
        k: 256,
        nb: 128,
        num_blocks: 4,
    };
    let data = Dataset::build(&p, &mh::epoch_seed(3), p.num_blocks, 2).unwrap();
    let hh = [9u8; 32];
    for nonce in 0..20u64 {
        for (height, gather_from) in [(10u64, u64::MAX), (10, 0)] {
            let one = mh::compute_attempt_at(&data, &hh, nonce, height, gather_from).unwrap();
            for threads in [2usize, 3, 4, 6, 7, 128, 1000] {
                let many =
                    mh::compute_attempt_at_threads(&data, &hh, nonce, height, gather_from, threads)
                        .unwrap();
                assert_eq!(
                    many, one,
                    "nonce {nonce}, {threads} threads, gather from {gather_from}"
                );
            }
        }
    }
}
