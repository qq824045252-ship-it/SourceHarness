use std::collections::{BTreeMap, HashSet};

use serde_json::{json, Value};
use source_harness::bench::{build_100_cases, BenchCategory, SizeTier};
use source_harness::registry;
use source_harness::store::{FoldStore, IngestOutcome};

#[test]
fn bench_100_cases_distribution_bounds_and_routing() {
    let cases = build_100_cases();
    assert_eq!(cases.len(), 100);

    let mut ids = HashSet::new();
    let mut tier_counts: BTreeMap<SizeTier, usize> = BTreeMap::new();
    let mut cat_counts: BTreeMap<BenchCategory, usize> = BTreeMap::new();

    for case in &cases {
        assert!(ids.insert(case.id.clone()), "duplicate id {}", case.id);
        let (min_b, max_b) = case.size_tier.byte_bounds();
        assert!(
            case.document.len() >= min_b && case.document.len() <= max_b,
            "{} size {} outside {:?}",
            case.id,
            case.document.len(),
            (min_b, max_b)
        );
        assert!(!case.checks.is_empty(), "{} has no checks", case.id);

        *tier_counts.entry(case.size_tier).or_insert(0) += 1;
        *cat_counts.entry(case.category).or_insert(0) += 1;

        let store = FoldStore::new();
        match (case.size_tier, store.ingest(case.document.clone())) {
            (SizeTier::K1, IngestOutcome::Inline(_)) => {}
            (SizeTier::K4 | SizeTier::K16, IngestOutcome::Folded(_)) => {}
            (tier, outcome) => panic!("{} ({:?}) routed unexpectedly: {:?}", case.id, tier, outcome),
        }
    }

    assert_eq!(tier_counts.get(&SizeTier::K1).copied(), Some(30));
    assert_eq!(tier_counts.get(&SizeTier::K4).copied(), Some(35));
    assert_eq!(tier_counts.get(&SizeTier::K16).copied(), Some(35));
    assert_eq!(cat_counts.len(), 10);
    for (cat, count) in cat_counts {
        assert_eq!(count, 10, "category {cat:?} count mismatch");
    }
}

#[test]
fn bench_sharded_and_sorted_ground_truth_self_consistency() {
    let cases = build_100_cases();

    // Verify 16k sharded pagination cases actually produce `complete: false` + `next_start_line`.
    let sharded_16k: Vec<_> = cases
        .iter()
        .filter(|c| c.category == BenchCategory::ShardedReadPagination && c.ideal_tool_rounds == 3)
        .collect();
    assert_eq!(sharded_16k.len(), 3);

    for case in sharded_16k {
        let store = FoldStore::new();
        let env = store.insert(case.document.clone());
        let data_sec = env
            .sections
            .iter()
            .find(|s| s.name == "Data Blocks")
            .expect("Data Blocks section missing");
        let section_len = data_sec.lines[1] - data_sec.lines[0] + 1;

        let mut start = data_sec.lines[0];
        let mut remaining = section_len;
        let mut pages = 0usize;
        let mut collected = String::new();

        while remaining > 0 {
            pages += 1;
            let raw = registry::dispatch(
                "read_result",
                &json!({
                    "result_ref": env.result_ref,
                    "start_line": start,
                    "line_count": remaining
                })
                .to_string(),
                &store,
            );
            let val: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(val["status"], "ok");
            collected.push_str(val["data"]["content"].as_str().unwrap());

            if val["data"]["complete"].as_bool().unwrap() {
                break;
            }
            let next_start = val["data"]["next_start_line"].as_u64().unwrap() as usize;
            let returned_end = val["data"]["returned_line_range"][1].as_u64().unwrap() as usize;
            assert_eq!(next_start, returned_end + 1);
            remaining -= returned_end - start + 1;
            start = next_start;
        }

        assert!(
            pages >= 2,
            "{} must require >=2 read_result shards (got {pages})",
            case.id
        );
        for check in &case.checks {
            check.verify(&collected).expect("check must match collected section");
        }
    }

    // Verify tiebreak sort order cases against search_result capture aggregation.
    for case in cases
        .iter()
        .filter(|c| c.category == BenchCategory::SortTiebreakAndDedup)
    {
        let store = FoldStore::new();
        let env = store.insert(case.document.clone());
        let raw = registry::dispatch(
            "search_result",
            &json!({
                "result_ref": env.result_ref,
                "query": "HOST=([A-Z0-9_]+)",
                "match_mode": "regex",
                "group_by_capture": 1,
                "top_k": 10
            })
            .to_string(),
            &store,
        );
        let val: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(val["status"], "ok");
        assert_eq!(val["data"]["aggregation"]["distinct_values"], 4);
        assert_eq!(val["data"]["aggregation"]["total_captured"], 18);
        let vc = val["data"]["aggregation"]["value_counts"].as_array().unwrap();
        let mut summary_text = format!(
            "distinct={} total={} ",
            val["data"]["aggregation"]["distinct_values"],
            val["data"]["aggregation"]["total_captured"]
        );
        summary_text.push_str(
            &vc.iter()
                .map(|e| format!("{}:{}", e["value"].as_str().unwrap(), e["count"]))
                .collect::<Vec<_>>()
                .join(" "),
        );
        for check in &case.checks {
            check.verify(&summary_text).expect("sort tiebreak check failed");
        }
    }
}
