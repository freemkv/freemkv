use super::*;

#[test]
fn saved_identity_rebinds_reordered_titles_before_key_selection() {
    let mut disc = crate::ku_fixture::bd_image().disc;
    disc.titles = crate::selection_test_fixtures::launch_titles();
    let mut saved = plan(&[1]);
    saved[0].title_identity = Some(crate::title_identity::TitleIdentity::of(&disc.titles[1]));
    let encoded = serde_json::to_vec(&saved).unwrap();
    let saved: Vec<staging::Output> = serde_json::from_slice(&encoded).unwrap();
    disc.titles.swap(0, 1);
    let rebound = rebind_resume_outputs(&disc.titles, &saved).unwrap();
    assert_eq!(rebound[0].title_index, 0);
    assert_eq!(resume_titles(&disc, false, &rebound).unwrap(), vec![0]);
    assert_eq!(
        saved[0].title_index, 1,
        "rebinding must not mutate saved evidence"
    );
    let rebound = rebind_resume_outputs(&disc.titles[..1], &saved).unwrap();
    assert_eq!(
        rebound[0].title_index, 0,
        "a stale out-of-range hint is not identity"
    );
}

#[test]
fn saved_identity_missing_duplicate_and_legacy_require_review() {
    let titles = crate::selection_test_fixtures::launch_titles();
    let mut saved = plan(&[0]);
    saved[0].title_identity = Some(crate::title_identity::TitleIdentity::of(&titles[0]));
    assert!(rebind_resume_outputs(&titles[1..], &saved).is_err());
    assert!(rebind_resume_outputs(&[titles[0].clone(), titles[0].clone()], &saved).is_err());
    assert!(rebind_resume_outputs(&titles, &plan(&[0])).is_err());
    assert!(rebind_resume_outputs(&titles, &[]).is_err());
    let legacy: Vec<staging::Output> = serde_json::from_str(r#"[{"filename":"old.mkv","title_index":0,"episode":null,"episode_name":"","moved":false}]"#).unwrap();
    assert!(rebind_resume_outputs(&titles, &legacy).is_err());
    let mut changed = titles.clone();
    changed[0].extents.push(libfreemkv::disc::Extent {
        start_lba: 42,
        sector_count: 1,
    });
    assert!(
        rebind_resume_outputs(&changed, &saved).is_err(),
        "same playlist but different bytes is not identity"
    );
}

#[test]
fn saved_identity_fanout_rebind_is_atomic_and_keeps_episode_order() {
    let mut titles = crate::selection_test_fixtures::launch_titles();
    let mut saved = plan(&[0, 1]);
    for (i, output) in saved.iter_mut().enumerate() {
        output.title_identity = Some(crate::title_identity::TitleIdentity::of(&titles[i]));
        output.episode = Some(i as u16 + 4);
    }
    assert!(rebind_resume_outputs(&titles[..1], &saved).is_err());
    assert_eq!(
        saved.iter().map(|o| o.title_index).collect::<Vec<_>>(),
        [0, 1]
    );
    titles.reverse();
    let rebound = rebind_resume_outputs(&titles, &saved).unwrap();
    assert_eq!(
        rebound
            .iter()
            .map(|o| (o.title_index, o.episode))
            .collect::<Vec<_>>(),
        [(1, Some(4)), (0, Some(5))]
    );
}

fn plan(indices: &[usize]) -> Vec<staging::Output> {
    indices
        .iter()
        .map(|&title_index| staging::Output {
            title_index,
            ..Default::default()
        })
        .collect()
}

// Legacy numeric hints never become key-service scope without identity proof.
#[test]
fn resume_titles_refuses_unproved_numeric_hints() {
    let disc = crate::ku_fixture::bd_image().disc;
    assert!(resume_titles(&disc, false, &plan(&[3])).is_err());
    assert!(resume_titles(&disc, true, &plan(&[5, 0, 0])).is_err());
    assert!(resume_titles(&disc, true, &plan(&[7, 9])).is_err());
    assert!(resume_titles(&disc, false, &plan(&[0])).is_err());
}
