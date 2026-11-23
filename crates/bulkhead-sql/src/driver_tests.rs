#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use rstest::rstest;

use super::*;

#[rstest]
#[case::one_worker(1)]
#[case::four(4)]
#[case::three_not_dividing_2_32(3)]
#[case::the_most(16)]
fn tx_number____every_n_and_its_wrap____names_its_worker(#[case] workers: u32) {
    let span = u32::MAX / workers;
    for worker in 0..workers {
        for n in [
            0,
            1,
            2,
            span - 2,
            span - 1,
            span,
            span.saturating_add(1),
            u32::MAX,
        ] {
            let (id, after) = tx_number(n, worker, workers);

            assert_eq!(tx_worker(id, workers as usize), worker as usize, "n {n}");
            assert!(after < span, "n {n}");
        }
    }
}

#[test]
fn tx_number____in_turn____distinct_until_the_span_wraps() {
    let mut n = 0;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..1000 {
        let (id, after) = tx_number(n, 2, 4);
        assert!(seen.insert(id));
        n = after;
    }
    assert_eq!(tx_number(u32::MAX / 4 - 1, 2, 4).1, 0);
}

#[rstest]
#[case::even(8, 4, [2, 2, 2, 2])]
#[case::remainder_to_the_first(10, 4, [3, 3, 2, 2])]
#[case::one_each(4, 4, [1, 1, 1, 1])]
fn pool_share____pool_among_workers____every_connection_once(
    #[case] pool: usize,
    #[case] workers: usize,
    #[case] want: [usize; 4],
) {
    let got: Vec<usize> = (0..workers).map(|i| pool_share(pool, workers, i)).collect();

    assert_eq!(got, want);
    assert_eq!(got.iter().sum::<usize>(), pool);
}

#[rstest]
#[case::all_idle(vec![0, 0, 0, 0], 0)]
#[case::the_first_busy(vec![1, 0, 0, 0], 1)]
#[case::ties_to_the_first(vec![2, 1, 1, 3], 1)]
#[case::none(vec![], 0)]
fn least_busy____in_flight_counts____the_first_of_the_fewest(
    #[case] counts: Vec<usize>,
    #[case] want: usize,
) {
    assert_eq!(least_busy(counts), want);
}
