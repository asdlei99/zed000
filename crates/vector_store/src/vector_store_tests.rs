use crate::{
    db::dot,
    embedding::EmbeddingProvider,
    parsing::{CodeContextRetriever, Document},
    vector_store_settings::VectorStoreSettings,
    VectorStore,
};
use anyhow::Result;
use async_trait::async_trait;
use gpui::{Task, TestAppContext};
use language::{Language, LanguageConfig, LanguageRegistry};
use project::{project_settings::ProjectSettings, FakeFs, Fs, Project};
use rand::{rngs::StdRng, Rng};
use serde_json::json;
use settings::SettingsStore;
use std::{
    path::Path,
    sync::{
        atomic::{self, AtomicUsize},
        Arc,
    },
};
use unindent::Unindent;

#[ctor::ctor]
fn init_logger() {
    if std::env::var("RUST_LOG").is_ok() {
        env_logger::init();
    }
}

#[gpui::test]
async fn test_vector_store(cx: &mut TestAppContext) {
    cx.update(|cx| {
        cx.set_global(SettingsStore::test(cx));
        settings::register::<VectorStoreSettings>(cx);
        settings::register::<ProjectSettings>(cx);
    });

    let fs = FakeFs::new(cx.background());
    fs.insert_tree(
        "/the-root",
        json!({
            "src": {
                "file1.rs": "
                    fn aaa() {
                        println!(\"aaaa!\");
                    }

                    fn zzzzzzzzz() {
                        println!(\"SLEEPING\");
                    }
                ".unindent(),
                "file2.rs": "
                    fn bbb() {
                        println!(\"bbbb!\");
                    }
                ".unindent(),
            }
        }),
    )
    .await;

    let languages = Arc::new(LanguageRegistry::new(Task::ready(())));
    let rust_language = rust_lang();
    languages.add(rust_language);

    let db_dir = tempdir::TempDir::new("vector-store").unwrap();
    let db_path = db_dir.path().join("db.sqlite");

    let embedding_provider = Arc::new(FakeEmbeddingProvider::default());
    let store = VectorStore::new(
        fs.clone(),
        db_path,
        embedding_provider.clone(),
        languages,
        cx.to_async(),
    )
    .await
    .unwrap();

    let project = Project::test(fs.clone(), ["/the-root".as_ref()], cx).await;
    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().unwrap().read(cx).id()
    });
    let file_count = store
        .update(cx, |store, cx| store.index_project(project.clone(), cx))
        .await
        .unwrap();
    assert_eq!(file_count, 2);
    cx.foreground().run_until_parked();
    store.update(cx, |store, _cx| {
        assert_eq!(
            store.remaining_files_to_index_for_project(&project),
            Some(0)
        );
    });

    let search_results = store
        .update(cx, |store, cx| {
            store.search_project(project.clone(), "aaaa".to_string(), 5, cx)
        })
        .await
        .unwrap();

    assert_eq!(search_results[0].byte_range.start, 0);
    assert_eq!(search_results[0].name, "aaa");
    assert_eq!(search_results[0].worktree_id, worktree_id);

    fs.save(
        "/the-root/src/file2.rs".as_ref(),
        &"
            fn dddd() { println!(\"ddddd!\"); }
            struct pqpqpqp {}
        "
        .unindent()
        .into(),
        Default::default(),
    )
    .await
    .unwrap();

    cx.foreground().run_until_parked();

    let prev_embedding_count = embedding_provider.embedding_count();
    let file_count = store
        .update(cx, |store, cx| store.index_project(project.clone(), cx))
        .await
        .unwrap();
    assert_eq!(file_count, 1);

    cx.foreground().run_until_parked();
    store.update(cx, |store, _cx| {
        assert_eq!(
            store.remaining_files_to_index_for_project(&project),
            Some(0)
        );
    });

    assert_eq!(
        embedding_provider.embedding_count() - prev_embedding_count,
        2
    );
}

#[gpui::test]
async fn test_code_context_retrieval() {
    let language = rust_lang();
    let mut retriever = CodeContextRetriever::new();

    let text = "
        /// A doc comment
        /// that spans multiple lines
        fn a() {
            b
        }

        impl C for D {
        }
    "
    .unindent();

    let parsed_files = retriever
        .parse_file(Path::new("foo.rs"), &text, language)
        .unwrap();

    assert_eq!(
        parsed_files,
        &[
            Document {
                name: "a".into(),
                range: text.find("fn a").unwrap()..(text.find("}").unwrap() + 1),
                content: "
                    The below code snippet is from file 'foo.rs'

                    ```rust
                    /// A doc comment
                    /// that spans multiple lines
                    fn a() {
                        b
                    }
                    ```"
                .unindent(),
                embedding: vec![],
            },
            Document {
                name: "C for D".into(),
                range: text.find("impl C").unwrap()..(text.rfind("}").unwrap() + 1),
                content: "
                    The below code snippet is from file 'foo.rs'

                    ```rust
                    impl C for D {
                    }
                    ```"
                .unindent(),
                embedding: vec![],
            }
        ]
    );
}

#[gpui::test]
fn test_dot_product(mut rng: StdRng) {
    assert_eq!(dot(&[1., 0., 0., 0., 0.], &[0., 1., 0., 0., 0.]), 0.);
    assert_eq!(dot(&[2., 0., 0., 0., 0.], &[3., 1., 0., 0., 0.]), 6.);

    for _ in 0..100 {
        let size = 1536;
        let mut a = vec![0.; size];
        let mut b = vec![0.; size];
        for (a, b) in a.iter_mut().zip(b.iter_mut()) {
            *a = rng.gen();
            *b = rng.gen();
        }

        assert_eq!(
            round_to_decimals(dot(&a, &b), 1),
            round_to_decimals(reference_dot(&a, &b), 1)
        );
    }

    fn round_to_decimals(n: f32, decimal_places: i32) -> f32 {
        let factor = (10.0 as f32).powi(decimal_places);
        (n * factor).round() / factor
    }

    fn reference_dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b.iter()).map(|(a, b)| a * b).sum()
    }
}

#[derive(Default)]
struct FakeEmbeddingProvider {
    embedding_count: AtomicUsize,
}

impl FakeEmbeddingProvider {
    fn embedding_count(&self) -> usize {
        self.embedding_count.load(atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl EmbeddingProvider for FakeEmbeddingProvider {
    async fn embed_batch(&self, spans: Vec<&str>) -> Result<Vec<Vec<f32>>> {
        self.embedding_count
            .fetch_add(spans.len(), atomic::Ordering::SeqCst);
        Ok(spans
            .iter()
            .map(|span| {
                let mut result = vec![1.0; 26];
                for letter in span.chars() {
                    let letter = letter.to_ascii_lowercase();
                    if letter as u32 >= 'a' as u32 {
                        let ix = (letter as u32) - ('a' as u32);
                        if ix < 26 {
                            result[ix as usize] += 1.0;
                        }
                    }
                }

                let norm = result.iter().map(|x| x * x).sum::<f32>().sqrt();
                for x in &mut result {
                    *x /= norm;
                }

                result
            })
            .collect())
    }
}

fn rust_lang() -> Arc<Language> {
    Arc::new(
        Language::new(
            LanguageConfig {
                name: "Rust".into(),
                path_suffixes: vec!["rs".into()],
                ..Default::default()
            },
            Some(tree_sitter_rust::language()),
        )
        .with_embedding_query(
            r#"
            (
                (line_comment)* @context
                .
                (enum_item
                    name: (_) @name) @item
            )

            (
                (line_comment)* @context
                .
                (struct_item
                    name: (_) @name) @item
            )

            (
                (line_comment)* @context
                .
                (impl_item
                    trait: (_)? @name
                    "for"? @name
                    type: (_) @name) @item
            )

            (
                (line_comment)* @context
                .
                (trait_item
                    name: (_) @name) @item
            )

            (
                (line_comment)* @context
                .
                (function_item
                    name: (_) @name) @item
            )

            (
                (line_comment)* @context
                .
                (macro_definition
                    name: (_) @name) @item
            )

            (
                (line_comment)* @context
                .
                (function_signature_item
                    name: (_) @name) @item
            )
            "#,
        )
        .unwrap(),
    )
}
