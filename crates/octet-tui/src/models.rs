//! Every provider's model list for `/model`: the cache, the probes that
//! refresh it, and how fresh each list is. Nothing here names a vendor.
use octet_core::{
    Engine, ModelInfo, SkillInfo,
    catalog::{self, Catalogs},
    skills::{self, SkillCatalogs},
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio::sync::mpsc;

/// A probe's result.
#[derive(Debug)]
pub(crate) struct Probed {
    pub(crate) engine: Engine,
    pub(crate) result: Result<octet_core::Listing, String>,
}

/// Where probes run and which CLI each provider's starts.
#[derive(Debug, Clone)]
pub(crate) struct Prober {
    pub(crate) cwd: PathBuf,
    /// The CLI a provider runs when it is not its default (`--binary`).
    pub(crate) binaries: HashMap<Engine, PathBuf>,
    pub(crate) limit: Duration,
    /// The run's plugin switches, so a probe loads what a session would.
    pub(crate) plugins: octet_core::Plugins,
}

impl Prober {
    /// The CLI to start for `engine`.
    fn binary(&self, engine: Engine) -> PathBuf {
        self.binaries
            .get(&engine)
            .cloned()
            .unwrap_or_else(|| PathBuf::from(engine.provider().default_binary))
    }
}

/// What `/model refresh` did: the probes it started, and those already
/// running.
#[derive(Debug, Default)]
pub(crate) struct Refreshed {
    pub(crate) started: Vec<Engine>,
    pub(crate) running: Vec<Engine>,
}

/// The lists, their cache file and the probes in flight.
#[derive(Debug)]
pub(crate) struct Models {
    pub(crate) catalogs: Catalogs,
    /// The cache file; `None` keeps the lists in memory only.
    path: Option<PathBuf>,
    /// Each provider's skills, per workspace.
    skill_catalogs: SkillCatalogs,
    /// Their cache file, beside `path`.
    skill_path: Option<PathBuf>,
    /// Set once the interface may start probes.
    prober: Option<Prober>,
    pub(crate) probing: HashSet<Engine>,
    /// Probes that failed this run: not retried until `/model refresh`.
    pub(crate) failed: HashMap<Engine, String>,
    tx: mpsc::UnboundedSender<Probed>,
    rx: mpsc::UnboundedReceiver<Probed>,
}

impl Default for Models {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            catalogs: Catalogs::default(),
            path: None,
            skill_catalogs: SkillCatalogs::default(),
            skill_path: None,
            prober: None,
            probing: HashSet::new(),
            failed: HashMap::new(),
            tx,
            rx,
        }
    }
}

impl Models {
    /// Loads the cache from `directory` and allows probes.
    pub(crate) fn attach(&mut self, directory: &Path, prober: Prober) {
        let path = directory.join(catalog::FILE);
        self.catalogs = Catalogs::load(&path);
        self.path = Some(path);
        let skill_path = directory.join(skills::FILE);
        self.skill_catalogs = SkillCatalogs::load(&skill_path);
        self.skill_path = Some(skill_path);
        self.prober = Some(prober);
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.path.is_some()
    }

    /// Probes every other provider whose list is missing or stale, unless a
    /// probe of it failed this run. The offline demo starts no CLI: there,
    /// only `/model refresh` fetches.
    pub(crate) fn probe_stale(&mut self, active: Engine, now: SystemTime) {
        if !active.is_vendor() {
            return;
        }
        for engine in others(active) {
            if self.catalogs.stale(engine, now) && !self.failed.contains_key(&engine) {
                self.probe(engine);
            }
        }
    }

    /// `/model refresh`: probes every other provider now, forgetting any
    /// earlier failure.
    pub(crate) fn refresh(&mut self, active: Engine) -> Refreshed {
        let mut refreshed = Refreshed::default();
        for engine in others(active) {
            self.failed.remove(&engine);
            if self.probing.contains(&engine) {
                refreshed.running.push(engine);
            } else if self.probe(engine) {
                refreshed.started.push(engine);
            }
        }
        refreshed
    }

    /// Starts a probe of `engine`; false when one runs, the provider cannot
    /// list its models, or probes are not allowed.
    fn probe(&mut self, engine: Engine) -> bool {
        let Some(prober) = self.prober.clone() else {
            return false;
        };
        if !engine.provider().lists_models || !self.probing.insert(engine) {
            return false;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let binary = prober.binary(engine);
            let config = octet_core::Config {
                plugins: prober.plugins,
                ..octet_core::Config::new(engine, binary, prober.cwd)
            };
            let result = octet_core::probe(config, prober.limit).await;
            let _ = tx.send(Probed { engine, result });
        });
        true
    }

    /// The next probe result. Waits forever while none runs; cancel-safe.
    pub(crate) async fn next(&mut self) -> Probed {
        match self.rx.recv().await {
            Some(probed) => probed,
            // Unreachable while `tx` lives, which it does as long as `self`.
            None => std::future::pending().await,
        }
    }

    /// A probe ended. A note when the lists could not be saved.
    pub(crate) fn probed(&mut self, probed: Probed, now: SystemTime) -> Option<String> {
        self.probing.remove(&probed.engine);
        match probed.result {
            Ok(listing) => {
                self.failed.remove(&probed.engine);
                self.catalogs.set(probed.engine, listing.models, now);
                let saved = self.save();
                // The probe ran in the workspace, so its skills are that one's.
                let skills_saved = self.prober.as_ref().map(|p| p.cwd.clone()).and_then(|ws| {
                    self.skill_catalogs
                        .set(probed.engine, &ws, listing.skills, now);
                    self.save_skills()
                });
                saved.or(skills_saved)
            }
            Err(error) => {
                self.failed.insert(probed.engine, error);
                None
            }
        }
    }

    /// The active provider's live list. A note when it could not be saved.
    pub(crate) fn live(
        &mut self,
        engine: Engine,
        models: Vec<ModelInfo>,
        now: SystemTime,
    ) -> Option<String> {
        // The demo's list is no provider's, and an empty one is nothing.
        if models.is_empty() || !engine.is_vendor() {
            return None;
        }
        // The same list, cached not long ago: no need to write it again (a
        // Codex catalog arrives page by page, with every reconnect).
        let fresh = |listed: &catalog::Listed| {
            now.duration_since(listed.fetched)
                .is_ok_and(|age| age < catalog::STALE_AFTER / 2)
        };
        if self
            .catalogs
            .get(engine)
            .is_some_and(|listed| listed.models == models && fresh(listed))
        {
            return None;
        }
        self.catalogs.set(engine, models, now);
        self.save()
    }

    /// The active provider's live skills for `workspace`. A note when they
    /// could not be saved.
    pub(crate) fn live_skills(
        &mut self,
        engine: Engine,
        workspace: &Path,
        skills: Vec<SkillInfo>,
        now: SystemTime,
    ) -> Option<String> {
        // The same list is not written again (Codex re-lists on every change).
        if !engine.is_vendor() || self.skill_catalogs.get(engine, workspace) == Some(&skills[..]) {
            return None;
        }
        self.skill_catalogs.set(engine, workspace, skills, now);
        self.save_skills()
    }

    /// `engine`'s skills for `workspace`, as last listed; empty when never.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the /skills picker, next, reads it")
    )]
    pub(crate) fn skills(&self, engine: Engine, workspace: &Path) -> &[SkillInfo] {
        self.skill_catalogs
            .get(engine, workspace)
            .unwrap_or_default()
    }

    fn save_skills(&mut self) -> Option<String> {
        let path = self.skill_path.as_ref()?;
        self.skill_catalogs
            .save(path)
            .err()
            .map(|error| format!("The skill lists could not be cached: {error}"))
    }

    fn save(&mut self) -> Option<String> {
        let path = self.path.as_ref()?;
        self.catalogs
            .save(path)
            .err()
            .map(|error| format!("The model lists could not be cached: {error}"))
    }

    /// How fresh `engine`'s list is, for the `/model` header. `live`: the
    /// connected session reported it.
    pub(crate) fn freshness(&self, engine: Engine, live: bool, now: SystemTime) -> String {
        if live {
            return "live".into();
        }
        if self.probing.contains(&engine) {
            return "probing…".into();
        }
        match (self.catalogs.get(engine), self.failed.get(&engine)) {
            (None, Some(error)) => {
                format!("unavailable: {}", error.lines().next().unwrap_or_default())
            }
            (None, None) => "not fetched".into(),
            (Some(listed), _) => format!("cached {}", ago(now, listed.fetched)),
        }
    }

    /// Model names for Tab: selections and full IDs once each, with their
    /// provider, the active provider's first.
    pub(crate) fn names(&self, active: Engine) -> Vec<(String, Engine)> {
        let mut seen = HashSet::new();
        std::iter::once(active)
            .chain(others(active))
            .flat_map(|engine| {
                self.catalogs
                    .get(engine)
                    .into_iter()
                    .flat_map(move |listed| {
                        listed.models.iter().flat_map(move |m| {
                            std::iter::once(m.selection.clone())
                                .chain(m.id.clone())
                                .map(move |name| (name, engine))
                        })
                    })
            })
            .filter(|(name, _)| seen.insert(name.clone()))
            .collect()
    }
}

/// Every vendor provider but `active`, in table order.
fn others(active: Engine) -> impl Iterator<Item = Engine> {
    Engine::ALL
        .iter()
        .copied()
        .filter(move |engine| engine.is_vendor() && *engine != active)
}

/// "just now", "5 min ago", "2 h ago", "3 days ago".
fn ago(now: SystemTime, then: SystemTime) -> String {
    let seconds = now.duration_since(then).map_or(0, |age| age.as_secs());
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_core::Engine;
    use std::time::{Duration, SystemTime};

    fn model(s: &str) -> ModelInfo {
        ModelInfo {
            selection: s.into(),
            id: Some(s.into()),
            name: s.into(),
            description: String::new(),
        }
    }
    /// Probes run the fake vendor for every provider.
    fn fake_prober() -> Prober {
        Prober {
            cwd: std::env::temp_dir(),
            binaries: Engine::ALL
                .iter()
                .map(|engine| (*engine, octet_testkit::protocol_child()))
                .collect(),
            limit: octet_core::PROBE_LIMIT,
            plugins: octet_core::Plugins::default(),
        }
    }
    fn attached(dir: &Path) -> Models {
        let mut models = Models::default();
        models.attach(dir, fake_prober());
        models
    }
    fn temp(name: &str) -> octet_testkit::TempDir {
        let dir = octet_testkit::TempDir::new(name);
        std::fs::create_dir_all(dir.path()).unwrap();
        dir
    }

    fn skill(name: &str) -> octet_core::SkillInfo {
        octet_core::SkillInfo {
            name: name.into(),
            description: String::new(),
            aliases: Vec::new(),
            argument_hint: String::new(),
            path: None,
        }
    }

    #[test]
    fn a_live_skills_list_is_cached_for_its_workspace() {
        let dir = temp("octet-models-live-skills");
        let mut models = attached(dir.path());
        let ws = Path::new("/work/a");
        assert!(
            models
                .live_skills(
                    Engine::CLAUDE,
                    ws,
                    vec![skill("brainstorm")],
                    SystemTime::now()
                )
                .is_none()
        );
        assert_eq!(models.skills(Engine::CLAUDE, ws)[0].name, "brainstorm");
        assert!(
            models
                .skills(Engine::CLAUDE, Path::new("/work/b"))
                .is_empty()
        );
        let cached =
            octet_core::skills::SkillCatalogs::load(&dir.path().join(octet_core::skills::FILE));
        assert_eq!(
            cached.get(Engine::CLAUDE, ws).unwrap()[0].name,
            "brainstorm"
        );
    }

    #[test]
    fn a_probes_skills_are_cached_for_the_probes_workspace() {
        let dir = temp("octet-models-probed-skills");
        let mut models = attached(dir.path());
        let listing = octet_core::Listing {
            models: vec![model("gpt-x")],
            skills: vec![skill("pdf")],
        };
        let probed = Probed {
            engine: Engine::CODEX,
            result: Ok(listing),
        };
        assert!(models.probed(probed, SystemTime::now()).is_none());
        // `fake_prober` probes in the temporary directory.
        let ws = std::env::temp_dir();
        assert_eq!(models.skills(Engine::CODEX, &ws)[0].name, "pdf");
        let cached =
            octet_core::skills::SkillCatalogs::load(&dir.path().join(octet_core::skills::FILE));
        assert_eq!(cached.get(Engine::CODEX, &ws).unwrap()[0].name, "pdf");
    }

    #[tokio::test]
    async fn a_stale_provider_is_probed_once_and_cached() {
        let dir = octet_testkit::TempDir::new("octet-models-probe");
        std::fs::create_dir_all(dir.path()).unwrap();
        let mut models = attached(dir.path());
        models.probe_stale(Engine::CLAUDE, SystemTime::now());
        // A second request while one runs starts nothing.
        assert!(models.refresh(Engine::CLAUDE).started.is_empty());
        let probed = models.next().await;
        assert_eq!(probed.engine, Engine::CODEX);
        assert!(models.probed(probed, SystemTime::now()).is_none());
        let cached = Catalogs::load(&dir.path().join(octet_core::catalog::FILE));
        assert!(
            cached
                .get(Engine::CODEX)
                .is_some_and(|l| !l.models.is_empty())
        );
        // Fresh now: no probe.
        models.probe_stale(Engine::CLAUDE, SystemTime::now());
        assert!(models.probing.is_empty());
    }

    #[tokio::test]
    async fn a_binary_override_is_the_cli_probed() {
        let dir = temp("octet-models-binary");
        let mut models = Models::default();
        // Only Codex has an override; Claude would run its default CLI.
        let mut prober = fake_prober();
        prober.binaries.retain(|engine, _| *engine == Engine::CODEX);
        models.attach(dir.path(), prober);
        models.probe_stale(Engine::CLAUDE, SystemTime::now());
        let probed = models.next().await;
        assert_eq!(probed.engine, Engine::CODEX);
        assert_eq!(probed.result.unwrap().models[0].selection, "fixture");
    }

    #[tokio::test]
    async fn refresh_says_which_lists_are_already_on_their_way() {
        let dir = temp("octet-models-refresh");
        let mut models = attached(dir.path());
        models.probe_stale(Engine::CLAUDE, SystemTime::now());
        let refreshed = models.refresh(Engine::CLAUDE);
        assert!(refreshed.started.is_empty());
        assert_eq!(refreshed.running, [Engine::CODEX]);
    }

    #[test]
    fn an_unchanged_live_list_is_not_saved_again() {
        let dir = temp("octet-models-unchanged");
        let mut models = attached(dir.path());
        let file = dir.path().join(octet_core::catalog::FILE);
        let now = SystemTime::now();
        assert!(
            models
                .live(Engine::CLAUDE, vec![model("opus")], now)
                .is_none()
        );
        assert!(file.exists());
        std::fs::remove_file(&file).unwrap();
        assert!(
            models
                .live(Engine::CLAUDE, vec![model("opus")], now)
                .is_none()
        );
        assert!(!file.exists(), "the same list was written again");
    }

    #[test]
    fn the_demo_list_is_not_cached() {
        let dir = temp("octet-models-demo-live");
        let mut models = attached(dir.path());
        assert!(
            models
                .live(Engine::DEMO, vec![model("demo")], SystemTime::now())
                .is_none()
        );
        assert!(models.catalogs.get(Engine::DEMO).is_none());
        assert!(!dir.path().join(octet_core::catalog::FILE).exists());
    }

    #[tokio::test]
    async fn the_offline_demo_probes_nothing() {
        let dir = octet_testkit::TempDir::new("octet-models-demo");
        std::fs::create_dir_all(dir.path()).unwrap();
        let mut models = attached(dir.path());
        models.probe_stale(Engine::DEMO, SystemTime::now());
        assert!(models.probing.is_empty(), "{:?}", models.probing);
    }

    #[test]
    fn a_failed_provider_is_not_probed_again_until_refresh() {
        let mut models = Models::default();
        models.failed.insert(Engine::CODEX, "not signed in".into());
        assert!(
            models
                .freshness(Engine::CODEX, false, SystemTime::now())
                .starts_with("unavailable: not signed in")
        );
        models.probe_stale(Engine::CLAUDE, SystemTime::now());
        assert!(models.probing.is_empty());
    }

    #[test]
    fn freshness_says_where_a_list_came_from() {
        let mut models = Models::default();
        let now = SystemTime::now();
        assert_eq!(models.freshness(Engine::CODEX, false, now), "not fetched");
        models.catalogs.set(
            Engine::CODEX,
            vec![model("a-1")],
            now - Duration::from_hours(2),
        );
        assert_eq!(
            models.freshness(Engine::CODEX, false, now),
            "cached 2 h ago"
        );
        assert_eq!(models.freshness(Engine::CODEX, true, now), "live");
    }

    #[test]
    fn names_put_the_active_provider_first_once_each() {
        let mut models = Models::default();
        let now = SystemTime::now();
        models
            .catalogs
            .set(Engine::CODEX, vec![model("gpt-6-astra")], now);
        models
            .catalogs
            .set(Engine::CLAUDE, vec![model("opus"), model("opus")], now);
        let names = models.names(Engine::CLAUDE);
        assert_eq!(
            names,
            [
                ("opus".into(), Engine::CLAUDE),
                ("gpt-6-astra".into(), Engine::CODEX)
            ]
        );
    }
}
