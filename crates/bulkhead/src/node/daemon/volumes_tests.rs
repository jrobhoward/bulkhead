#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use bulkhead_bundle::manifest::{AppInfo, AppSpec, ByteSize, ProgramRef, Provenance};
use rstest::rstest;

use super::*;
use crate::node::config::ZfsVolumes;

fn manifest(preopens: serde_json::Value) -> AppManifest {
    AppManifest {
        app: AppInfo {
            name: "orders".into(),
            version: semver::Version::new(1, 0, 0),
            description: None,
            authors: vec![],
            license: None,
            repository: None,
        },
        program: ProgramRef {
            path: "orders".into(),
            checksum: "sha256:00".into(),
        },
        provenance: Provenance {
            signer_key: "RWQ".into(),
            build_info: None,
            sbom: None,
        },
        spec: AppSpec::from_json_value(
            &serde_json::json!({ "capabilities": { "bulkhead:files": { "preopens": preopens } } }),
        )
        .unwrap(),
    }
}

fn volumes(zfs: bool) -> Volumes {
    Volumes {
        dirs: BTreeMap::from([("logs".to_string(), PathBuf::from("/srv/logs"))]),
        zfs: zfs.then(|| ZfsVolumes {
            parent: "tank/v".into(),
            max_quota: "1GiB".parse::<ByteSize>().unwrap(),
        }),
    }
}

#[test]
fn sources____a_directory_and_a_zfs_volume____each_from_its_place() {
    let m = manifest(serde_json::json!({
        "/logs": { "volume": "logs", "mode": "rw" },
        "/data": { "volume": "data", "mode": "rw", "quota": "64MiB" },
    }));

    let s = sources(&volumes(true), &m).unwrap();

    assert_eq!(s["logs"], Source::Dir(PathBuf::from("/srv/logs")));
    assert_eq!(s["data"], Source::Zfs { refquota: 64 << 20 });
}

#[test]
fn sources____no_filesystem_scope____none() {
    let mut m = manifest(serde_json::json!({}));
    m.spec.capabilities.filesystem = None;

    assert!(sources(&volumes(true), &m).unwrap().is_empty());
}

#[rstest]
#[case(true, serde_json::json!({ "/logs": { "volume": "logs", "quota": "1MiB" } }), "enforces no quota")]
#[case(true, serde_json::json!({ "/data": { "volume": "data", "mode": "rw" } }), "needs a quota")]
#[case(true, serde_json::json!({ "/data": { "volume": "data", "quota": "2GiB" } }), "max_quota")]
#[case(false, serde_json::json!({ "/data": { "volume": "data", "quota": "1MiB" } }), "not configured on this node")]
#[case(true, serde_json::json!({
    "/a": { "volume": "data", "quota": "1MiB" },
    "/b": { "volume": "data", "quota": "2MiB" },
}), "named twice")]
fn sources____against_the_rules____refused_saying_why(
    #[case] zfs: bool,
    #[case] preopens: serde_json::Value,
    #[case] says: &str,
) {
    let err = sources(&volumes(zfs), &manifest(preopens)).unwrap_err();

    assert!(err.contains(says), "{err}");
}

#[test]
fn metrics____two_volumes____three_families_labelled_by_app_and_volume() {
    let row = |app: &str, volume: &str| VolumeRow {
        app: app.into(),
        volume: volume.into(),
        used: 3,
        referenced: 2,
        refquota: 64,
        mountpoint: "/srv/v".into(),
        mounted: true,
    };
    let kept: Kept = Arc::new(Mutex::new(vec![row("orders", "data"), row("logs", "a")]));

    let text = metrics(&kept);

    assert!(text.contains("# TYPE bulkhead_volume_referenced_bytes gauge\n"));
    assert!(text.contains("bulkhead_volume_referenced_bytes{app=\"orders\",volume=\"data\"} 2\n"));
    assert!(text.contains("bulkhead_volume_used_bytes{app=\"logs\",volume=\"a\"} 3\n"));
    assert!(text.contains("bulkhead_volume_quota_bytes{app=\"orders\",volume=\"data\"} 64\n"));
}

#[test]
fn metrics____no_volumes____nothing() {
    assert_eq!(metrics(&Kept::default()), "");
}
