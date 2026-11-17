mod path_trie {
    use std::{collections::BTreeSet, ops::ControlFlow};

    use util::rel_path::rel_path;

    use project::manifest_tree::path_trie::*;

    #[test]
    fn test_insert_and_lookup() {
        let mut trie = RootPathTrie::<()>::new();
        trie.insert(
            &TriePath::new(rel_path("a/b/c")),
            (),
            LabelPresence::Present,
        );

        trie.walk(&TriePath::new(rel_path("a/b/c")), &mut |path, nodes| {
            assert_eq!(nodes.get(&()), Some(&LabelPresence::Present));
            assert_eq!(path.as_unix_str(), "a/b/c");
            ControlFlow::Continue(())
        });
        // Now let's annotate a parent with "Known missing" node.
        trie.insert(
            &TriePath::new(rel_path("a")),
            (),
            LabelPresence::KnownAbsent,
        );

        // Ensure that we walk from the root to the leaf.
        let mut visited_paths = BTreeSet::new();
        trie.walk(&TriePath::new(rel_path("a/b/c")), &mut |path, nodes| {
            if path.as_unix_str() == "a/b/c" {
                assert_eq!(visited_paths, BTreeSet::from_iter([rel_path("a").into()]));
                assert_eq!(nodes.get(&()), Some(&LabelPresence::Present));
            } else if path.as_unix_str() == "a" {
                assert!(visited_paths.is_empty());
                assert_eq!(nodes.get(&()), Some(&LabelPresence::KnownAbsent));
            } else {
                panic!("Unknown path");
            }
            // Assert that we only ever visit a path once.
            assert!(visited_paths.insert(path.clone()));
            ControlFlow::Continue(())
        });

        // One can also pass a path whose prefix is in the tree, but not that path itself.
        let mut visited_paths = BTreeSet::new();
        trie.walk(
            &TriePath::new(rel_path("a/b/c/d/e/f/g")),
            &mut |path, nodes| {
                if path.as_unix_str() == "a/b/c" {
                    assert_eq!(visited_paths, BTreeSet::from_iter([rel_path("a").into()]));
                    assert_eq!(nodes.get(&()), Some(&LabelPresence::Present));
                } else if path.as_unix_str() == "a" {
                    assert!(visited_paths.is_empty());
                    assert_eq!(nodes.get(&()), Some(&LabelPresence::KnownAbsent));
                } else {
                    panic!("Unknown path");
                }
                // Assert that we only ever visit a path once.
                assert!(visited_paths.insert(path.clone()));
                ControlFlow::Continue(())
            },
        );

        // Test breaking from the tree-walk.
        let mut visited_paths = BTreeSet::new();
        trie.walk(&TriePath::new(rel_path("a/b/c")), &mut |path, nodes| {
            if path.as_unix_str() == "a" {
                assert!(visited_paths.is_empty());
                assert_eq!(nodes.get(&()), Some(&LabelPresence::KnownAbsent));
            } else {
                panic!("Unknown path");
            }
            // Assert that we only ever visit a path once.
            assert!(visited_paths.insert(path.clone()));
            ControlFlow::Break(())
        });
        assert_eq!(visited_paths.len(), 1);

        // Entry removal.
        trie.insert(
            &TriePath::new(rel_path("a/b")),
            (),
            LabelPresence::KnownAbsent,
        );
        let mut visited_paths = BTreeSet::new();
        trie.walk(&TriePath::new(rel_path("a/b/c")), &mut |path, _nodes| {
            // Assert that we only ever visit a path once.
            assert!(visited_paths.insert(path.clone()));
            ControlFlow::Continue(())
        });
        assert_eq!(visited_paths.len(), 3);
        trie.remove(&TriePath::new(rel_path("a/b")));
        let mut visited_paths = BTreeSet::new();
        trie.walk(&TriePath::new(rel_path("a/b/c")), &mut |path, _nodes| {
            // Assert that we only ever visit a path once.
            assert!(visited_paths.insert(path.clone()));
            ControlFlow::Continue(())
        });
        assert_eq!(visited_paths.len(), 1);
        assert_eq!(
            visited_paths.into_iter().next().unwrap(),
            rel_path("a").into()
        );
    }

    #[test]
    fn path_to_a_root_can_contain_multiple_known_nodes() {
        let mut trie = RootPathTrie::<()>::new();
        trie.insert(&TriePath::new(rel_path("a/b")), (), LabelPresence::Present);
        trie.insert(&TriePath::new(rel_path("a")), (), LabelPresence::Present);
        let mut visited_paths = BTreeSet::new();
        trie.walk(&TriePath::new(rel_path("a/b/c")), &mut |path, nodes| {
            assert_eq!(nodes.get(&()), Some(&LabelPresence::Present));
            if path.as_unix_str() != "a" && path.as_unix_str() != "a/b" {
                panic!("Unexpected path: {}", path.as_unix_str());
            }
            assert!(visited_paths.insert(path.clone()));
            ControlFlow::Continue(())
        });
        assert_eq!(visited_paths.len(), 2);
    }

    // FORK: Cover stale negative caches when an ancestor manifest is discovered (H25).
    #[test]
    fn discovering_ancestor_root_clears_only_matching_absent_descendants() {
        let mut trie = RootPathTrie::<&str>::new();
        trie.insert(
            &TriePath::new(rel_path("project/src/main.rs")),
            "cargo",
            LabelPresence::KnownAbsent,
        );
        trie.insert(
            &TriePath::new(rel_path("project/src/main.rs")),
            "python",
            LabelPresence::KnownAbsent,
        );
        trie.insert(
            &TriePath::new(rel_path("project/nested")),
            "cargo",
            LabelPresence::Present,
        );
        trie.insert(
            &TriePath::new(rel_path("project/nested/src/lib.rs")),
            "cargo",
            LabelPresence::KnownAbsent,
        );
        trie.insert(
            &TriePath::new(rel_path("other/main.rs")),
            "cargo",
            LabelPresence::KnownAbsent,
        );
        trie.insert(
            &TriePath::new(rel_path("project")),
            "cargo",
            LabelPresence::Present,
        );

        let mut visited = Vec::new();
        trie.walk(
            &TriePath::new(rel_path("project/src/main.rs")),
            &mut |path, labels| {
                visited.push((path.clone(), labels.clone()));
                ControlFlow::Continue(())
            },
        );
        assert_eq!(
            visited,
            vec![
                (
                    rel_path("project").into(),
                    [("cargo", LabelPresence::Present)].into()
                ),
                (
                    rel_path("project/src/main.rs").into(),
                    [("python", LabelPresence::KnownAbsent)].into()
                ),
            ]
        );

        let mut visited = Vec::new();
        trie.walk(
            &TriePath::new(rel_path("project/nested/src/lib.rs")),
            &mut |path, labels| {
                visited.push((path.clone(), labels.clone()));
                ControlFlow::Continue(())
            },
        );
        assert_eq!(
            visited,
            vec![
                (
                    rel_path("project").into(),
                    [("cargo", LabelPresence::Present)].into()
                ),
                (
                    rel_path("project/nested").into(),
                    [("cargo", LabelPresence::Present)].into()
                ),
            ]
        );
        trie.walk(
            &TriePath::new(rel_path("other/main.rs")),
            &mut |_, labels| {
                assert_eq!(labels.get("cargo"), Some(&LabelPresence::KnownAbsent));
                ControlFlow::Continue(())
            },
        );
    }

    #[test]
    fn discovering_root_promotes_an_absent_node() {
        let mut trie = RootPathTrie::<()>::new();
        let path = TriePath::new(rel_path("project"));
        trie.insert(&path, (), LabelPresence::KnownAbsent);
        trie.insert(&path, (), LabelPresence::Present);
        let mut visited = Vec::new();
        trie.walk(&path, &mut |path, labels| {
            visited.push((path.clone(), labels.clone()));
            ControlFlow::Continue(())
        });
        assert_eq!(
            visited,
            vec![(
                rel_path("project").into(),
                [((), LabelPresence::Present)].into()
            )]
        );
    }

    #[test]
    fn discovering_worktree_root_clears_absent_descendants() {
        let mut trie = RootPathTrie::<()>::new();
        trie.insert(
            &TriePath::new(rel_path("src/main.rs")),
            (),
            LabelPresence::KnownAbsent,
        );
        trie.insert(
            &TriePath::new(rel_path("tests/test.rs")),
            (),
            LabelPresence::KnownAbsent,
        );
        trie.insert(&TriePath::new(rel_path("")), (), LabelPresence::Present);
        for path in ["src/main.rs", "tests/test.rs"] {
            let mut visited = Vec::new();
            trie.walk(&TriePath::new(rel_path(path)), &mut |path, labels| {
                visited.push((path.clone(), labels.clone()));
                ControlFlow::Continue(())
            });
            assert_eq!(
                visited,
                vec![(rel_path("").into(), [((), LabelPresence::Present)].into())]
            );
        }
    }
}
