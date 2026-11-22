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
