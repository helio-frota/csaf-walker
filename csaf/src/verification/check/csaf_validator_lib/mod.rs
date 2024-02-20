//! A validator based on the `csaf_validator_lib`

mod deno;

use crate::verification::check::{Check, CheckError};
use anyhow::anyhow;
use async_trait::async_trait;
use csaf::Csaf;
use deno_core::{
    op2, serde_v8, v8, Extension, JsRuntime, ModuleCodeString, Op, PollEventLoopOptions,
    RuntimeOptions, StaticModuleLoader,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Debug;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar};
use std::time::Duration;
use tokio::sync::Mutex;
use url::Url;

const MODULE_ID: &'static str = "internal://bundle.js";

#[derive(Default)]
pub struct FunctionsState {
    pub runner_func: Option<v8::Global<v8::Function>>,
}

#[op2]
pub fn op_register_func(
    #[state] function_state: &mut FunctionsState,
    #[global] f: v8::Global<v8::Function>,
) {
    function_state.runner_func.replace(f);
}

struct InnerCheck {
    runtime: JsRuntime,
    runner: v8::Global<v8::Function>,
}

impl InnerCheck {
    pub async fn new() -> anyhow::Result<Self> {
        let specifier = Url::parse(MODULE_ID).expect("internal module ID must parse");
        #[cfg(debug_assertions)]
        let code = include_str!("js/bundle.debug.js");
        #[cfg(not(debug_assertions))]
        let code = include_str!("js/bundle.js");

        let ext = Extension {
            ops: std::borrow::Cow::Borrowed(&[op_register_func::DECL]),
            op_state_fn: Some(Box::new(|state| {
                state.put(FunctionsState::default());
            })),
            ..Default::default()
        };

        let mut runtime = JsRuntime::new(RuntimeOptions {
            module_loader: Some(Rc::new(StaticModuleLoader::with(
                specifier,
                ModuleCodeString::Static(code),
            ))),
            extensions: vec![ext],
            ..Default::default()
        });

        let module = Url::parse(MODULE_ID)?;
        let mod_id = runtime.load_main_module(&module, None).await?;
        let result = runtime.mod_evaluate(mod_id);
        runtime
            .run_event_loop(PollEventLoopOptions::default())
            .await?;

        result.await?;

        let state: FunctionsState = runtime.op_state().borrow_mut().take();
        let runner = state
            .runner_func
            .ok_or_else(|| anyhow!("runner function was not initialized"))?;

        Ok(InnerCheck { runtime, runner })
    }

    async fn validate<S, D>(
        &mut self,
        doc: S,
        validations: &[ValidationSet],
        timeout: Option<Duration>,
    ) -> anyhow::Result<Option<D>>
    where
        S: Serialize + Send,
        D: for<'de> Deserialize<'de> + Send + Default + Debug,
    {
        log::debug!("Create arguments");

        let args = {
            let scope = &mut self.runtime.handle_scope();

            let doc = {
                let doc = serde_v8::to_v8(scope, doc)?;
                v8::Global::new(scope, doc)
            };

            let validations = {
                let validations = serde_v8::to_v8(scope, validations)?;
                v8::Global::new(scope, validations)
            };

            [validations, doc]
        };

        let deadline = timeout.map(|duration| {
            log::debug!("Starting deadline");
            let isolate = self.runtime.v8_isolate().thread_safe_handle();

            let lock = Arc::new((
                std::sync::Mutex::new(()),
                Condvar::new(),
                AtomicBool::new(false),
            ));
            {
                let lock = lock.clone();
                std::thread::spawn(move || {
                    let (lock, notify, cancelled) = &*lock;
                    let lock = lock.lock().expect("unable to acquire deadline lock");
                    log::debug!("Deadline active");
                    let (_lock, result) = notify
                        .wait_timeout(lock, duration)
                        .expect("unable to await deadline");

                    if result.timed_out() {
                        log::info!("Terminating execution after: {duration:?}");
                        cancelled.store(true, Ordering::Release);
                        isolate.terminate_execution();
                    } else {
                        log::debug!("Deadline cancelled");
                    }
                });
            }

            Deadline(lock)
        });

        log::debug!("Call function");

        let call = self.runtime.call_with_args(&self.runner, &args);

        log::debug!("Wait for completion");

        let result = self
            .runtime
            .with_event_loop_promise(call, PollEventLoopOptions::default())
            .await;

        // first check if we got cancelled

        let cancelled = deadline
            .as_ref()
            .map(|deadline| deadline.was_cancelled())
            .unwrap_or_default();

        drop(deadline);

        if cancelled {
            return Ok(None);
        }

        // now process the result

        let result = result?;

        log::debug!("Extract result");

        let result = {
            let scope = &mut self.runtime.handle_scope();
            let result = v8::Local::new(scope, result);
            let result: D = serde_v8::from_v8(scope, result)?;

            result
        };

        log::trace!("Result: {result:#?}");

        Ok(Some(result))
    }
}

struct Deadline(Arc<(std::sync::Mutex<()>, Condvar, AtomicBool)>);

impl Deadline {
    pub fn was_cancelled(&self) -> bool {
        let (_, _, cancelled) = &*self.0;
        cancelled.load(Ordering::Acquire)
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        log::debug!("Aborting deadline");
        let (_lock, notify, _cancelled) = &*self.0;
        notify.notify_one();
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ValidationSet {
    Schema,
    Mandatory,
    Optional,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Profile {
    Schema,
    Mandatory,
    Optional,
}

pub struct CsafValidatorLib {
    runtime: Arc<Mutex<Option<InnerCheck>>>,
    validations: Vec<ValidationSet>,
    timeout: Option<Duration>,
}

impl CsafValidatorLib {
    pub fn new(profile: Profile) -> Self {
        let runtime = Arc::new(Mutex::new(None));

        let validations = match profile {
            Profile::Schema => vec![ValidationSet::Schema],
            Profile::Mandatory => vec![ValidationSet::Schema, ValidationSet::Mandatory],
            Profile::Optional => vec![
                ValidationSet::Schema,
                ValidationSet::Mandatory,
                ValidationSet::Optional,
            ],
        };

        Self {
            runtime,
            validations,
            timeout: None,
        }
    }

    pub fn timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.timeout = timeout.into();
        self
    }

    pub fn with_timeout(mut self, timeout: impl Into<Duration>) -> Self {
        self.timeout = Some(timeout.into());
        self
    }

    pub fn without_timeout(mut self) -> Self {
        self.timeout = None;
        self
    }
}

#[async_trait(?Send)]
impl Check for CsafValidatorLib {
    async fn check(&self, csaf: &Csaf) -> anyhow::Result<Vec<CheckError>> {
        let mut inner_lock = self.runtime.lock().await;

        let inner = match &mut *inner_lock {
            Some(inner) => inner,
            None => {
                let new = InnerCheck::new().await?;
                inner_lock.get_or_insert(new)
            }
        };

        let test_result = inner
            .validate::<_, TestResult>(csaf, &self.validations, self.timeout)
            .await?;

        log::trace!("Result: {test_result:?}");

        let Some(test_result) = test_result else {
            // clear instance, and return timeout
            inner_lock.take();
            return Ok(vec!["check timed out".into()]);
        };

        let mut result = vec![];

        for entry in test_result.tests {
            // we currently only report "failed" tests
            if entry.is_valid {
                continue;
            }

            for error in entry.errors {
                result.push(
                    format!(
                        "{name}: {message}",
                        name = entry.name,
                        message = error.message
                    )
                    .into(),
                );
            }
        }

        Ok(result)
    }
}

/// Result structure, coming from the test call
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestResult {
    pub tests: Vec<Entry>,
}

/// Test result entry from the tests
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    pub name: String,
    pub is_valid: bool,

    pub errors: Vec<Error>,
    pub warnings: Vec<Value>,
    pub infos: Vec<Value>,
}

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Error {
    pub message: String,
}

#[cfg(test)]
mod test {
    use super::*;
    use csaf::document::*;
    use log::LevelFilter;
    use std::borrow::Cow;
    use std::io::BufReader;

    fn valid_doc() -> Csaf {
        serde_json::from_reader(BufReader::new(
            std::fs::File::open("tests/good.json").expect("must be able to open file"),
        ))
        .expect("must parse")
    }

    fn invalid_doc() -> Csaf {
        Csaf {
            document: Document {
                category: Category::Base,
                publisher: Publisher {
                    category: PublisherCategory::Coordinator,
                    name: "".to_string(),
                    namespace: Url::parse("http://example.com").expect("test URL must parse"),
                    contact_details: None,
                    issuing_authority: None,
                },
                title: "".to_string(),
                tracking: Tracking {
                    current_release_date: Default::default(),
                    id: "".to_string(),
                    initial_release_date: Default::default(),
                    revision_history: vec![],
                    status: Status::Draft,
                    version: "".to_string(),
                    aliases: None,
                    generator: None,
                },
                csaf_version: CsafVersion::TwoDotZero,
                acknowledgments: None,
                aggregate_severity: None,
                distribution: None,
                lang: None,
                notes: None,
                references: None,
                source_lang: None,
            },
            product_tree: None,
            vulnerabilities: None,
        }
    }

    #[tokio::test]
    async fn basic_test() {
        let _ = env_logger::builder()
            .filter_level(LevelFilter::Info)
            .try_init();

        let check = CsafValidatorLib::new(Profile::Optional);

        let result = check.check(&invalid_doc()).await;

        log::info!("Result: {result:#?}");

        let result = result.expect("must succeed");

        assert!(!result.is_empty());
    }

    /// run twice to ensure we can re-use the runtime
    #[tokio::test]
    async fn test_twice() {
        let _ = env_logger::builder()
            .filter_level(LevelFilter::Info)
            .try_init();

        let check = CsafValidatorLib::new(Profile::Optional);

        let result = check.check(&invalid_doc()).await;
        log::info!("Result: {result:#?}");
        let result = result.expect("must succeed");
        assert!(!result.is_empty());

        let result = check.check(&invalid_doc()).await;

        log::info!("Result: {result:#?}");
        let result = result.expect("must succeed");
        assert!(!result.is_empty());
    }

    #[tokio::test]
    async fn test_ok() {
        let _ = env_logger::builder()
            .filter_level(LevelFilter::Info)
            .try_init();

        let check = CsafValidatorLib::new(Profile::Optional);

        let result = check.check(&valid_doc()).await;
        log::info!("Result: {result:#?}");
        let result = result.expect("must succeed");
        assert_eq!(result, Vec::<CheckError>::new());
    }

    #[tokio::test]
    #[ignore]
    async fn test_timeout() {
        let _ = env_logger::builder().try_init();

        log::info!("Loading file");

        let doc = serde_json::from_reader(BufReader::new(
            std::fs::File::open("../data/rhsa-2018_3140.json").expect("test file should open"),
        ))
        .expect("test file should parse");

        log::info!("Creating instance");

        let check = CsafValidatorLib::new(Profile::Optional).with_timeout(Duration::from_secs(10));

        log::info!("Running check");

        let result = check.check(&doc).await;
        log::info!("Result: {result:#?}");
        let result = result.expect("must succeed");
        assert_eq!(result, vec![Cow::Borrowed("check timed out")]);
    }

    #[tokio::test]
    async fn test_timeout_next() {
        let _ = env_logger::builder().try_init();

        log::info!("Loading file");

        let doc = serde_json::from_reader(BufReader::new(
            std::fs::File::open("../data/rhsa-2018_3140.json").expect("test file should open"),
        ))
        .expect("test file should parse");

        log::info!("Creating instance");

        let check = CsafValidatorLib::new(Profile::Optional).with_timeout(Duration::from_secs(10));

        log::info!("Running check");

        let result = check.check(&doc).await;
        log::info!("Result: {result:#?}");
        let result = result.expect("must succeed");
        assert_eq!(result, vec![Cow::Borrowed("check timed out")]);

        let result = check.check(&valid_doc()).await;
        log::info!("Result: {result:#?}");
        let result = result.expect("must succeed");
        assert!(result.is_empty());
    }
}
