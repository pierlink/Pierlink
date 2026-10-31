use super::*;
#[test]
fn f12_同秒等长内容变化拒绝覆盖且新版本包含原字节哈希() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("文本");
    fs::write(&path, b"\xef\xbb\xbfaaa").unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let loaded = read_text(&path, 100).unwrap();
    assert_eq!(
        loaded.hash.as_deref(),
        Some(text::hash(b"\xef\xbb\xbfaaa").as_str())
    );
    let expected = FileVersion {
        size: loaded.size,
        mtime: loaded.mtime,
        hash: loaded.hash,
    };
    fs::write(&path, b"\xef\xbb\xbfbbb").unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert_eq!(
        write_text(&path, "mine", true, Some(expected))
            .unwrap_err()
            .code,
        "files.changed"
    );
    assert_eq!(fs::read(&path).unwrap(), b"\xef\xbb\xbfbbb");
    let written = write_text(&path, "mine", true, None).unwrap();
    assert_eq!(written.hash, read_text(&path, 100).unwrap().hash);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}
#[test]
fn 文件创建重命名与名字校验() {
    let root = crate::test_support::tempdir();
    let a = root.path().join("甲.txt");
    let b = root.path().join("乙.txt");
    create_file(&a).unwrap();
    create_file(&b).unwrap();
    assert_eq!(create_file(&a).unwrap_err().code, "fs.exists");
    let e = rename(&a, &b).unwrap_err();
    assert_eq!(e.code, "fs.exists");
    assert_eq!(e.params["path"], super::super::display_path(&b));
    assert!(a.exists());
    assert_eq!(delete(&[]).unwrap(), 0);
    let c = root.path().join("丙.txt");
    rename(&a, &c).unwrap();
    assert!(!a.exists());
    let dir = root.path().join("目录");
    create_dir(&dir).unwrap();
    assert_eq!(create_dir(&dir).unwrap_err().code, "fs.exists");
    for name in ["CON", "a?", "..", ".", "a.", ""] {
        assert_eq!(
            create_file(&root.path().join(name)).unwrap_err().code,
            "files.invalid_name",
            "{name}"
        );
    }
    assert_eq!(open_with_default(&a).unwrap_err().code, "fs.not_found");
    assert_eq!(reveal_in_explorer(&a).unwrap_err().code, "fs.not_found");
}
#[test]
fn 文本三种拒绝bom往返与版本冲突() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("文本");
    for (bytes, max, code) in [
        (b"1234".as_slice(), 3, "files.too_large"),
        (b"a\0b", 9, "files.not_text"),
        (b"\xff", 9, "files.not_text"),
    ] {
        fs::write(&path, bytes).unwrap();
        assert_eq!(read_text(&path, max).unwrap_err().code, code);
    }
    fs::write(&path, b"\xef\xbb\xbfhello\r\n").unwrap();
    let loaded = read_text(&path, 100).unwrap();
    assert!(loaded.bom);
    assert_eq!(loaded.text, "hello\r\n");
    let expected = FileVersion {
        hash: loaded.hash.clone(),
        size: loaded.size,
        mtime: loaded.mtime,
    };
    let updated = write_text(&path, "中文\r\n", true, Some(expected.clone())).unwrap();
    assert_eq!(updated.size, 11);
    assert_eq!(fs::read(&path).unwrap(), "\u{feff}中文\r\n".as_bytes());
    assert_eq!(
        write_text(&path, "覆盖", false, Some(expected.clone()))
            .unwrap_err()
            .code,
        "files.changed"
    );
    write_text(&path, "覆盖", false, None).unwrap();
    assert!(!read_text(&path, 100).unwrap().bom);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    let latest = read_text(&path, 100).unwrap();
    let changed_time = FileVersion {
        hash: None,
        size: latest.size,
        mtime: latest.mtime - 1,
    };
    assert_eq!(
        write_text(&path, "x", false, Some(changed_time.clone()))
            .unwrap_err()
            .code,
        "files.changed"
    );
    assert_eq!(
        write_text(
            &root.path().join("已被移动"),
            "x",
            false,
            Some(changed_time.clone())
        )
        .unwrap_err()
        .code,
        "files.changed"
    );
}
/// Windows 真实测试：只改文件名大小写可以改（不当作重名），改成另一个已存在的名字仍然报错
#[cfg(windows)]
#[test]
fn windows_只改大小写的重命名() {
    let root = crate::test_support::tempdir();
    let from = root.path().join("readme.txt");
    create_file(&from).unwrap();
    create_file(&root.path().join("其他.txt")).unwrap();
    rename(&from, &root.path().join("README.txt")).unwrap();
    let names: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"README.txt".to_owned()), "{names:?}");
    assert_eq!(
        rename(
            &root.path().join("README.txt"),
            &root.path().join("其他.txt")
        )
        .unwrap_err()
        .code,
        "fs.exists"
    );
}
/// Windows 真实测试：删除走回收站（SHFileOperationW），文件和整个目录树都会消失，返回删除的项数
#[cfg(windows)]
#[test]
fn windows_删除移入回收站() {
    let root = crate::test_support::tempdir();
    let tree = root.path().join("回收 测试");
    create_dir(&tree).unwrap();
    create_file(&tree.join("里面.txt")).unwrap();
    let file = root.path().join("单个 文件.txt");
    create_file(&file).unwrap();
    assert_eq!(delete(&[tree.clone(), file.clone()]).unwrap(), 2);
    assert!(!tree.exists());
    assert!(!file.exists());
    assert_eq!(
        delete(&[root.path().join("不存在")]).unwrap_err().code,
        "fs.not_found"
    );
}
#[cfg(windows)]
#[test]
fn 只读目标替换失败后临时文件仍被清理() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("只读文本");
    fs::write(&path, "old").unwrap();
    let original = fs::metadata(&path).unwrap().permissions();
    let mut readonly = original.clone();
    readonly.set_readonly(true);
    fs::set_permissions(&path, readonly).unwrap();
    let result = write_text(&path, "new", false, None);
    let count = fs::read_dir(root.path()).unwrap().count();
    fs::set_permissions(&path, original).unwrap();
    assert_eq!(result.unwrap_err().code, "fs.permission_denied");
    assert_eq!(count, 1);
    assert_eq!(fs::read(&path).unwrap(), b"old");
}
#[cfg(unix)]
#[test]
fn 删除树不跟随链接并保持写入权限() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = crate::test_support::tempdir();
    let outside = root.path().join("外部");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("保留"), "data").unwrap();
    let tree = root.path().join("树");
    create_dir(&tree).unwrap();
    create_dir(&tree.join("子目录")).unwrap();
    create_file(&tree.join("子目录/文件")).unwrap();
    symlink(&outside, tree.join("链接")).unwrap();
    assert_eq!(delete(&[tree]).unwrap(), 4);
    assert_eq!(fs::read(outside.join("保留")).unwrap(), b"data");
    let path = outside.join("保留");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
    write_text(&path, "new", false, None).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o751
    );
    let link = root.path().join("文件链接");
    symlink(&path, &link).unwrap();
    write_text(&link, "target", false, None).unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&path).unwrap(), b"target");
    let error = delete(&[root.path().join("缺失")]).unwrap_err();
    assert_eq!(error.code, "fs.not_found");
    assert!(error.params["path"].ends_with("缺失"));
    assert_eq!(delete(&[]).unwrap(), 0);
}
