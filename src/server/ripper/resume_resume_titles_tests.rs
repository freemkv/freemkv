use super::*;

fn plan(indices: &[usize]) -> Vec<staging::Output> {
    indices
        .iter()
        .map(|&title_index| staging::Output {
            title_index,
            ..Default::default()
        })
        .collect()
}

// A movie keys title 0; a TV plan its episodes in range, once each, else title 0.
#[test]
fn resume_titles_keeps_in_range_plan_titles_once() {
    let disc = crate::ku_fixture::bd_image().disc;
    assert_eq!(resume_titles(&disc, false, &plan(&[3])), vec![0]);
    assert_eq!(resume_titles(&disc, true, &plan(&[5, 0, 0])), vec![0]);
    assert_eq!(resume_titles(&disc, true, &plan(&[7, 9])), vec![0]);
}
