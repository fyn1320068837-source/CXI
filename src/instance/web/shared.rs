//! Site adapters own tabs; one coordinator owns the persistent browser process.
use super::*;
use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

struct SharedBrowser {
    browser: WebAdapter,
    handle: Option<ProcessHandle>,
    tabs: HashMap<WebSite, String>,
}

pub(crate) struct SharedWebAdapter {
    site: WebSite,
    shared: Arc<AsyncMutex<SharedBrowser>>,
    legacy_gemini_root: String,
}

impl SharedWebAdapter {
    pub(crate) fn pair(
        home: &str,
        geo: SharedGeoProfile,
    ) -> (Arc<dyn InstanceAdapter>, Arc<dyn InstanceAdapter>) {
        let shared = Arc::new(AsyncMutex::new(SharedBrowser {
            browser: WebAdapter::new_with_geo_profile(format!("{home}/.cim/instances/web"), geo),
            handle: None,
            tabs: HashMap::new(),
        }));
        let make = |site| {
            Arc::new(Self {
                site,
                shared: shared.clone(),
                legacy_gemini_root: format!("{home}/.cim/instances/gemini-web"),
            }) as Arc<dyn InstanceAdapter>
        };
        (make(WebSite::Claude), make(WebSite::Gemini))
    }

    async fn open(&self) -> Result<ProcessHandle> {
        let mut state = self.shared.lock().await;
        if let Some(handle) = state.handle.clone() {
            let port = *state
                .browser
                .debug_ports
                .lock()
                .unwrap()
                .get(&handle.root_pid)
                .context("shared browser DevTools port missing")?;
            if let Some(id) = state.tabs.get(&self.site) {
                if cdp::tab_exists(port, id).await? {
                    cdp::tab_action(port, "activate", id).await?;
                    return Ok(handle);
                }
            }
            let profile = state.browser.geo_profile.read().await.clone();
            let id = cdp::open_site_tab(port, self.site, &profile, handle.root_pid).await?;
            state.tabs.insert(self.site, id);
            return Ok(handle);
        }
        // Preparation and launch are serialized across both application adapters.
        // Never remove profile locks or rewrite Preferences while Chrome is live.
        state.browser.site = self.site;
        state.browser.prepare().await?;
        let handle = state.browser.launch().await?;
        let id = state.browser.initial_target.lock().unwrap().clone();
        let Some(id) = id else {
            state.browser.stop(&handle).await?;
            anyhow::bail!("shared browser did not report its initial tab");
        };
        state.tabs.insert(self.site, id);
        state.handle = Some(handle.clone());
        Ok(handle)
    }
}

#[async_trait]
impl InstanceAdapter for SharedWebAdapter {
    fn set_request_log_sender(&mut self, sender: RequestLogSender) {
        self.shared
            .try_lock()
            .expect("log sink configured before launch")
            .browser
            .set_request_log_sender(sender);
    }
    fn owns_process_tree(&self) -> bool {
        false
    }
    async fn focus(&self) -> Result<()> {
        self.open().await.map(|_| ())
    }
    async fn detect(&self) -> CapabilityReport {
        self.shared.lock().await.browser.detect().await
    }
    async fn prepare(&self) -> Result<()> {
        Ok(())
    }
    async fn launch(&self) -> Result<ProcessHandle> {
        self.open().await
    }
    async fn cleanup_orphaned_sessions(&self) -> Result<()> {
        if self.site == WebSite::Claude {
            let state = self.shared.lock().await;
            state.browser.cleanup_orphaned_sessions().await?;
            // Retire old independent Gemini processes but preserve their profile.
            WebAdapter::new_gemini(
                self.legacy_gemini_root.clone(),
                state.browser.geo_profile.clone(),
            )
            .cleanup_orphaned_sessions()
            .await?;
        }
        Ok(())
    }
    async fn open_auth_url(&self, url: &str) -> Result<()> {
        anyhow::ensure!(
            self.site == WebSite::Claude,
            "Desktop authentication requires Claude Web"
        );
        let url = validate_auth_url(url)?;
        let state = self.shared.lock().await;
        let handle = state
            .handle
            .as_ref()
            .context("shared browser is not running")?;
        let port = *state
            .browser
            .debug_ports
            .lock()
            .unwrap()
            .get(&handle.root_pid)
            .context("shared browser port missing")?;
        // Existing bridge validates the destination; it must never navigate a sibling tab.
        cdp::open_url_tab(
            port,
            url.as_str(),
            &*state.browser.geo_profile.read().await,
            handle.root_pid,
        )
        .await?;
        Ok(())
    }
    async fn stop(&self, handle: &ProcessHandle) -> Result<()> {
        let mut state = self.shared.lock().await;
        // Concurrent duplicate stop/rollback calls must never close a sibling.
        if !state.tabs.contains_key(&self.site) {
            return Ok(());
        }
        anyhow::ensure!(
            state.handle.as_ref().map(|h| h.root_pid) == Some(handle.root_pid),
            "shared browser handle changed"
        );
        if state.tabs.len() <= 1 {
            state.browser.stop(handle).await?;
            state.tabs.clear();
            state.handle = None;
        } else if let Some(id) = state.tabs.get(&self.site).cloned() {
            let port = *state
                .browser
                .debug_ports
                .lock()
                .unwrap()
                .get(&handle.root_pid)
                .context("shared browser port missing")?;
            if cdp::tab_exists(port, &id).await? {
                cdp::tab_action(port, "close", &id).await?;
            }
            state.tabs.remove(&self.site);
        }
        Ok(())
    }
    async fn process_tree(&self, handle: &ProcessHandle) -> Vec<u32> {
        self.shared.lock().await.browser.process_tree(handle).await
    }
    async fn process_tree_checked(&self, handle: &ProcessHandle) -> Result<Vec<u32>> {
        self.shared
            .lock()
            .await
            .browser
            .process_tree_checked(handle)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};

    async fn fixture(close_ok: bool) -> (SharedWebAdapter, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route(
                "/json/list",
                get(|| async {
                    Json(json!([
                        {"id":"claude-tab","type":"page"}, {"id":"gemini-tab","type":"page"}
                    ]))
                }),
            )
            .route(
                "/json/close/:id",
                get(move || async move {
                    if close_ok {
                        axum::http::StatusCode::OK
                    } else {
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR
                    }
                }),
            )
            .route("/json/activate/:id", get(|| async { "ok" }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let browser = WebAdapter::new_with_geo_profile(
            "/fixture/shared-web".into(),
            crate::geoip::shared_profile(),
        );
        browser.debug_ports.lock().unwrap().insert(999_999, port);
        let adapter = SharedWebAdapter {
            site: WebSite::Gemini,
            legacy_gemini_root: "/fixture/legacy-gemini".into(),
            shared: Arc::new(AsyncMutex::new(SharedBrowser {
                browser,
                handle: Some(ProcessHandle { root_pid: 999_999 }),
                tabs: HashMap::from([
                    (WebSite::Claude, "claude-tab".into()),
                    (WebSite::Gemini, "gemini-tab".into()),
                ]),
            })),
        };
        (adapter, task)
    }

    #[tokio::test]
    async fn closing_gemini_keeps_claude_and_browser_alive() {
        let (adapter, task) = fixture(true).await;
        adapter
            .stop(&ProcessHandle { root_pid: 999_999 })
            .await
            .unwrap();
        let state = adapter.shared.lock().await;
        assert_eq!(state.handle.as_ref().unwrap().root_pid, 999_999);
        assert!(state.tabs.contains_key(&WebSite::Claude));
        assert!(!state.tabs.contains_key(&WebSite::Gemini));
        assert!(state
            .browser
            .debug_ports
            .lock()
            .unwrap()
            .contains_key(&999_999));
        assert!(!adapter.owns_process_tree());
        drop(state);
        adapter
            .stop(&ProcessHandle { root_pid: 999_999 })
            .await
            .unwrap();
        assert!(adapter
            .shared
            .lock()
            .await
            .tabs
            .contains_key(&WebSite::Claude));
        task.abort();
    }

    #[tokio::test]
    async fn tab_close_failure_preserves_retryable_ownership() {
        let (adapter, task) = fixture(false).await;
        assert!(adapter
            .stop(&ProcessHandle { root_pid: 999_999 })
            .await
            .is_err());
        assert_eq!(adapter.shared.lock().await.tabs.len(), 2);
        task.abort();
    }

    #[tokio::test]
    async fn repeated_open_focuses_existing_tab_and_preserves_shared_pid() {
        let (adapter, task) = fixture(true).await;
        assert_eq!(adapter.open().await.unwrap().root_pid, 999_999);
        assert_eq!(adapter.shared.lock().await.tabs.len(), 2);
        task.abort();
    }

    #[test]
    fn pair_uses_claude_profile_without_migrating_gemini_credentials() {
        let pair = SharedWebAdapter::pair("/fixture", crate::geoip::shared_profile());
        assert!(!pair.0.owns_process_tree());
        assert!(!pair.1.owns_process_tree());
    }
}
