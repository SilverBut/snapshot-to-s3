use super::support::{
    configure_bins, dataset_json, env_lock, fixture_dir, invalid_documents, property, write_json,
};
use serde_json::{json, Value};
use snapshot_to_s3::model::{Reader, SnapshotName};
use snapshot_to_s3::zfs::{SystemZfs, Zfs};
use std::fs;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn json_preserves_maximum_guid_and_numeric_properties() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-max-guid").unwrap();
    configure_bins(&base);
    let maximum = u64::MAX.to_string();
    write_json(
        &base,
        "get-guid",
        &dataset_json(
            "zfs get",
            "pool/fs",
            "FILESYSTEM",
            json!({"guid": property(&maximum)}),
        ),
    );
    write_json(
        &base,
        "get-snapshot",
        &dataset_json(
            "zfs get",
            "pool/fs@s2",
            "SNAPSHOT",
            json!({"guid": property(&maximum), "createtxg": property(&maximum)}),
        ),
    );
    let z = SystemZfs::new();
    let current = SnapshotName::parse("pool/fs@s2").unwrap();
    let info = z.snapshot(&current).await.unwrap();
    assert_eq!(info.guid, maximum);
    assert_eq!(info.volume_guid, maximum);
    assert_eq!(info.createtxg, u64::MAX);

    write_json(
        &base,
        "get-written",
        &dataset_json(
            "zfs get",
            "pool/fs@s2",
            "SNAPSHOT",
            json!({"written@s1": property(&maximum)}),
        ),
    );
    fs::remove_file(base.join("get-snapshot.json")).unwrap();
    let previous = SnapshotName::parse("pool/fs@s1").unwrap();
    assert_eq!(
        z.written(&previous, &current).await.unwrap(),
        Some(u64::MAX)
    );
}

#[tokio::test]
async fn json_snapshot_rejects_invalid_documents_and_required_properties() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-invalid-snapshot").unwrap();
    configure_bins(&base);
    let valid = dataset_json(
        "zfs get",
        "pool/fs@s2",
        "SNAPSHOT",
        json!({"guid": property("11"), "createtxg": property("101")}),
    );
    let z = SystemZfs::new();
    let current = SnapshotName::parse("pool/fs@s2").unwrap();
    for (label, value) in invalid_documents(&valid, "datasets", "pool/fs@s2") {
        write_json(&base, "get-snapshot", &value);
        assert!(z.snapshot(&current).await.is_err(), "{label} was accepted");
    }
    for key in ["guid", "createtxg"] {
        let mut value = valid.clone();
        value["datasets"]["pool/fs@s2"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove(key);
        write_json(&base, "get-snapshot", &value);
        assert!(
            z.snapshot(&current).await.is_err(),
            "missing {key} was accepted"
        );
        for invalid in [
            json!({}),
            json!({"value": null}),
            json!({"value": 11}),
            property(""),
            property("-1"),
            property("18446744073709551616"),
            property("1.0"),
            property("1K"),
        ] {
            let mut value = valid.clone();
            value["datasets"]["pool/fs@s2"]["properties"][key] = invalid.clone();
            write_json(&base, "get-snapshot", &value);
            assert!(
                z.snapshot(&current).await.is_err(),
                "{key}: {invalid} was accepted"
            );
        }
    }
    let mut zero_guid = valid.clone();
    zero_guid["datasets"]["pool/fs@s2"]["properties"]["guid"] = property("0");
    write_json(&base, "get-snapshot", &zero_guid);
    assert!(z.snapshot(&current).await.is_err());
    fs::write(base.join("get-snapshot.json"), "{not json").unwrap();
    assert!(z.snapshot(&current).await.is_err());
}

#[tokio::test]
async fn json_dataset_type_rejects_invalid_success_instead_of_treating_it_as_absent() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-invalid-type").unwrap();
    configure_bins(&base);
    let valid = dataset_json(
        "zfs get",
        "pool/fs",
        "FILESYSTEM",
        json!({"type": property("filesystem")}),
    );
    let z = SystemZfs::new();
    for (label, value) in invalid_documents(&valid, "datasets", "pool/fs") {
        write_json(&base, "get-type", &value);
        assert!(z.target("pool/fs").await.is_err(), "{label} was accepted");
    }
    for invalid in [
        json!({}),
        json!({"type": {}}),
        json!({"type": {"value": null}}),
    ] {
        let mut value = valid.clone();
        value["datasets"]["pool/fs"]["properties"] = invalid;
        write_json(&base, "get-type", &value);
        assert!(z.target("pool/fs").await.is_err());
    }
    fs::write(base.join("get-type.json"), "filesystem\n").unwrap();
    assert!(z.target("pool/fs").await.is_err());
    let commands = fs::read_to_string(base.join("commands")).unwrap();
    assert!(
        commands.lines().all(|line| {
            line == "zpool list -j -p -o name pool" || line == "get -j -p type pool/fs"
        }),
        "invalid JSON triggered an unexpected fallback: {commands}"
    );
}

#[tokio::test]
async fn json_filesystem_guid_requires_a_valid_decimal_string() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-invalid-guid").unwrap();
    configure_bins(&base);
    let current = SnapshotName::parse("pool/fs@s2").unwrap();
    let z = SystemZfs::new();
    for invalid in [
        json!({}),
        json!({"guid": {}}),
        json!({"guid": {"value": 22}}),
        json!({"guid": property("0")}),
        json!({"guid": property("18446744073709551616")}),
    ] {
        write_json(
            &base,
            "get-guid",
            &dataset_json("zfs get", "pool/fs", "FILESYSTEM", invalid),
        );
        assert!(z.snapshot(&current).await.is_err());
    }
}

#[tokio::test]
async fn json_written_requires_a_valid_property_value() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-invalid-written").unwrap();
    configure_bins(&base);
    let previous = SnapshotName::parse("pool/fs@s1").unwrap();
    let current = SnapshotName::parse("pool/fs@s2").unwrap();
    let z = SystemZfs::new();
    for invalid in [
        json!({}),
        json!({"written@s1": {}}),
        json!({"written@s1": {"value": 4096}}),
        json!({"written@s1": property("-")}),
        json!({"written@s1": property("-1")}),
        json!({"written@s1": property("18446744073709551616")}),
    ] {
        write_json(
            &base,
            "get-written",
            &dataset_json("zfs get", "pool/fs@s2", "SNAPSHOT", invalid),
        );
        assert!(z.written(&previous, &current).await.is_err());
    }
    write_json(
        &base,
        "get-written",
        &dataset_json(
            "zfs get",
            "pool/fs@s2",
            "SNAPSHOT",
            json!({"written@s1": property("0")}),
        ),
    );
    assert_eq!(z.written(&previous, &current).await.unwrap(), Some(0));
}

#[tokio::test]
async fn json_snapshot_list_validates_identity_and_accepts_empty_listing() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-invalid-list").unwrap();
    configure_bins(&base);
    let z = SystemZfs::new();
    let valid = dataset_json("zfs list", "pool/fs@s1", "SNAPSHOT", json!({}));
    for (label, value) in invalid_documents(&valid, "datasets", "pool/fs@s1") {
        if label == "missing requested object" {
            continue;
        }
        write_json(&base, "list", &value);
        assert!(
            z.snapshots("pool/fs").await.is_err(),
            "{label} was accepted"
        );
    }
    write_json(&base, "list", &valid);
    let snapshots = z.snapshots("pool/fs").await.unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].guid, "11");
    assert_eq!(snapshots[0].createtxg, 101);
    write_json(
        &base,
        "list",
        &json!({"output_version": {"command": "zfs list", "vers_major": 0, "vers_minor": 1}, "datasets": {}}),
    );
    assert!(z.snapshots("pool/fs").await.unwrap().is_empty());
}

#[tokio::test]
async fn json_decoding_ignores_unused_envelope_fields() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-unused-fields").unwrap();
    configure_bins(&base);
    let z = SystemZfs::new();
    let current = SnapshotName::parse("pool/fs@s2").unwrap();
    for version in [
        None,
        Some(Value::Null),
        Some(json!({"command": "anything", "vers_major": 99, "vers_minor": "future"})),
    ] {
        let mut snapshot = dataset_json(
            "zfs get",
            "pool/fs@s2",
            "SNAPSHOT",
            json!({"guid": property("18446744073709551615"), "createtxg": property("101")}),
        );
        let mut pool = json!({"pools": {"pool": {"name": "pool", "type": "POOL"}}});
        snapshot.as_object_mut().unwrap().remove("output_version");
        if let Some(version) = version {
            snapshot["output_version"] = version.clone();
            pool["output_version"] = version;
        }
        snapshot["unrelated"] = json!({"future": true});
        pool["unrelated"] = json!({"future": true});
        write_json(&base, "get-snapshot", &snapshot);
        write_json(&base, "pool", &pool);
        assert_eq!(
            z.snapshot(&current).await.unwrap().guid,
            "18446744073709551615"
        );
        assert!(!z.target("pool/missing").await.unwrap().exists);
    }
    write_json(
        &base,
        "list",
        &json!({"datasets": {"pool/fs@s2": {"name": "pool/fs@s2", "type": "SNAPSHOT"}}}),
    );
    assert_eq!(z.snapshots("pool/fs").await.unwrap().len(), 1);
}

#[tokio::test]
async fn json_pool_list_validates_identity() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-invalid-pool").unwrap();
    configure_bins(&base);
    let z = SystemZfs::new();
    let valid = json!({
        "output_version": {"command": "zpool list", "vers_major": 0, "vers_minor": 1},
        "pools": {"pool": {"name": "pool", "type": "POOL", "state": "ONLINE",
                          "pool_guid": "22", "properties": {}}}
    });
    for (label, value) in invalid_documents(&valid, "pools", "pool") {
        if matches!(label, "properties not object" | "missing properties") {
            continue;
        }
        write_json(&base, "pool", &value);
        assert!(z.target("pool/fs").await.is_err(), "{label} was accepted");
    }
    fs::write(base.join("pool.json"), "pool\n").unwrap();
    assert!(z.target("pool/fs").await.is_err());
}

#[tokio::test]
async fn json_metadata_keeps_diff_estimates_and_streams_in_native_formats() {
    let _lock = env_lock().await;
    let base = fixture_dir("json-native-streams").unwrap();
    configure_bins(&base);
    let previous = SnapshotName::parse("pool/fs@s1").unwrap();
    let current = SnapshotName::parse("pool/fs@s2").unwrap();
    let z = SystemZfs::new();
    z.check_clean(&current).await.unwrap();
    assert_eq!(z.written(&previous, &current).await.unwrap(), Some(4096));
    assert_eq!(z.estimate(&current, None).await.unwrap(), Some(8192));
    assert_eq!(
        z.estimate(&current, Some(&previous)).await.unwrap(),
        Some(8192)
    );
    for base in [None, Some(&previous)] {
        let mut stream = z.send(&current, base).await.unwrap();
        let mut bytes = Vec::new();
        stream.reader.read_to_end(&mut bytes).await.unwrap();
        stream.completion.await.unwrap().unwrap();
        assert_eq!(bytes, b"stream-data\n");
    }
    let mut input: Reader = Box::new(&b"binary\0stream\xff"[..]);
    z.receive("pool/new", &mut input).await.unwrap();
    let commands = fs::read_to_string(base.join("commands")).unwrap();
    for command in [
        "diff -H pool/fs@s2 pool/fs",
        "send -nP -w pool/fs@s2",
        "send -nP -w -i pool/fs@s1 pool/fs@s2",
        "send -w pool/fs@s2",
        "send -w -i pool/fs@s1 pool/fs@s2",
        "receive -u pool/new",
    ] {
        assert!(
            commands.lines().any(|line| line == command),
            "missing command: {command}"
        );
    }
}
