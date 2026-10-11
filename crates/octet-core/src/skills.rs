//! Each provider's skills as its CLI last listed them, per workspace (skills
//! depend on the folder), cached on disk beside the model lists so `/skills`
//! can offer every provider's. Saved and merged as `catalog` saves models.
use crate::{
    Engine, SkillInfo,
    catalog::{cached_text, fetched_at, seconds, write_private},
};
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// The cache file's name, in Octet's data directory.
pub const FILE: &str = "skills.json";
/// The most skills kept per list, as the drivers cap a live one.
const MAX_SKILLS: usize = 512;

/// Every provider's last skills list, per workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillCatalogs {
    lists: HashMap<(Engine, PathBuf), (Vec<SkillInfo>, SystemTime)>,
}

impl SkillCatalogs {
    /// The lists cached at `path`. A missing or malformed file, or a bad
    /// entry in it, is skipped.
    pub fn load(path: &Path) -> Self {
        let Some(root) = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        else {
            return Self::default();
        };
        let mut lists = HashMap::new();
        for (key, entry) in root.as_object().into_iter().flatten() {
            let Some((name, workspace)) = key.split_once('\t') else {
                continue;
            };
            let Some(engine) = Engine::parse(name).filter(|engine| engine.is_vendor()) else {
                continue;
            };
            let Some(fetched) = fetched_at(&entry["fetched"]) else {
                continue;
            };
            let skills = entry["skills"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(skill_from_json)
                .take(MAX_SKILLS)
                .collect();
            lists.insert((engine, PathBuf::from(workspace)), (skills, fetched));
        }
        Self { lists }
    }

    /// Writes the lists to `path`, privately and atomically. A list another
    /// window saved there more recently is kept, and adopted here too. A
    /// workspace whose path is not UTF-8 is kept for this run only.
    ///
    /// # Errors
    ///
    /// The file could not be written or renamed.
    pub fn save(&mut self, path: &Path) -> io::Result<()> {
        for (key, theirs) in Self::load(path).lists {
            if self.lists.get(&key).is_none_or(|mine| mine.1 < theirs.1) {
                self.lists.insert(key, theirs);
            }
        }
        let mut root = Map::new();
        for ((engine, workspace), (skills, fetched)) in &self.lists {
            let Some(workspace) = workspace.to_str() else {
                continue;
            };
            let skills: Vec<Value> = skills.iter().map(skill_to_json).collect();
            root.insert(
                format!("{}\t{workspace}", engine.as_str()),
                json!({"fetched": seconds(*fetched), "skills": skills}),
            );
        }
        let bytes = serde_json::to_vec(&Value::Object(root)).map_err(io::Error::other)?;
        write_private(path, &bytes)
    }

    /// Replaces `engine`'s list for `workspace`.
    pub fn set(
        &mut self,
        engine: Engine,
        workspace: &Path,
        mut skills: Vec<SkillInfo>,
        at: SystemTime,
    ) {
        skills.truncate(MAX_SKILLS);
        self.lists
            .insert((engine, workspace.to_path_buf()), (skills, at));
    }

    /// `engine`'s list for `workspace`, if one was ever fetched.
    pub fn get(&self, engine: Engine, workspace: &Path) -> Option<&[SkillInfo]> {
        self.lists
            .get(&(engine, workspace.to_path_buf()))
            .map(|(skills, _)| skills.as_slice())
    }
}

/// A cached skill, if it is a usable one: a name that splits back out of
/// `/name args`.
fn skill_from_json(value: &Value) -> Option<SkillInfo> {
    let usable = |name: &&str| {
        octet_engine::live::valid_identifier(name) && !name.chars().any(char::is_whitespace)
    };
    let name = value["name"].as_str().filter(usable)?;
    Some(SkillInfo {
        name: name.to_owned(),
        description: cached_text(&value["description"], 2048),
        aliases: value["aliases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(usable)
            .take(16)
            .map(str::to_owned)
            .collect(),
        argument_hint: cached_text(&value["argumentHint"], 512),
        path: value["path"]
            .as_str()
            .filter(|path| octet_engine::live::valid_identifier(path))
            .map(PathBuf::from),
    })
}

fn skill_to_json(skill: &SkillInfo) -> Value {
    json!({
        "name": skill.name,
        "description": skill.description,
        "aliases": skill.aliases,
        "argumentHint": skill.argument_hint,
        "path": skill.path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn skill(name: &str) -> SkillInfo {
        SkillInfo {
            name: name.into(),
            description: format!("{name} does a thing"),
            aliases: vec![format!("{name}-alias")],
            argument_hint: "[topic]".into(),
            path: Some(format!("/skills/{name}/SKILL.md").into()),
        }
    }
    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }
    fn sample() -> SkillCatalogs {
        let mut c = SkillCatalogs::default();
        c.set(
            Engine::CLAUDE,
            Path::new("/work/a"),
            vec![skill("brainstorm")],
            at(1_000_000),
        );
        c.set(
            Engine::CODEX,
            Path::new("/work/a"),
            vec![skill("pdf")],
            at(1_000_000),
        );
        c
    }
    fn file(name: &str) -> (octet_testkit::TempDir, std::path::PathBuf) {
        let dir = octet_testkit::TempDir::new(name);
        std::fs::create_dir_all(dir.path()).unwrap();
        let path = dir.path().join(FILE);
        (dir, path)
    }

    #[test]
    fn the_cache_round_trips_privately() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = file("octet-skills");
        sample().save(&path).unwrap();
        assert_eq!(SkillCatalogs::load(&path), sample());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn two_workspaces_are_kept_apart() {
        let mut c = sample();
        c.set(
            Engine::CODEX,
            Path::new("/work/b"),
            vec![skill("other")],
            at(5),
        );
        assert_eq!(
            c.get(Engine::CODEX, Path::new("/work/a")).unwrap()[0].name,
            "pdf"
        );
        assert_eq!(
            c.get(Engine::CODEX, Path::new("/work/b")).unwrap()[0].name,
            "other"
        );
        assert!(c.get(Engine::CLAUDE, Path::new("/work/b")).is_none());
    }

    #[test]
    fn a_malformed_cache_is_empty_and_bad_entries_are_skipped() {
        let (_dir, path) = file("octet-skills-bad");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(SkillCatalogs::load(&path), SkillCatalogs::default());
        std::fs::write(
            &path,
            r#"{"codex\t/w":{"fetched":5,"skills":[{"name":"ok"},{"name":"two words"},{"name":7}]},
                "demo\t/w":{"fetched":5,"skills":[{"name":"x"}]},
                "codex":{"fetched":5,"skills":[{"name":"x"}]},
                "claude\t/w":{"fetched":"soon","skills":[]}}"#,
        )
        .unwrap();
        let loaded = SkillCatalogs::load(&path);
        let codex = loaded.get(Engine::CODEX, Path::new("/w")).unwrap();
        assert_eq!(codex.len(), 1);
        assert_eq!(codex[0].name, "ok");
        assert!(loaded.get(Engine::CLAUDE, Path::new("/w")).is_none());
        assert!(loaded.get(Engine::DEMO, Path::new("/w")).is_none());
    }

    #[test]
    fn a_time_past_the_clock_is_skipped() {
        let (_dir, path) = file("octet-skills-far");
        std::fs::write(
            &path,
            r#"{"codex\t/w":{"fetched":18446744073709551615,"skills":[{"name":"ok"}]}}"#,
        )
        .unwrap();
        assert!(
            SkillCatalogs::load(&path)
                .get(Engine::CODEX, Path::new("/w"))
                .is_none()
        );
    }

    #[test]
    fn a_save_keeps_a_newer_list_from_another_window() {
        let (_dir, path) = file("octet-skills-merge");
        let mut other = SkillCatalogs::default();
        other.set(
            Engine::CODEX,
            Path::new("/work/a"),
            vec![skill("newer")],
            at(2_000_000),
        );
        other.save(&path).unwrap();
        let mut mine = sample();
        mine.save(&path).unwrap();
        let saved = SkillCatalogs::load(&path);
        assert_eq!(
            saved.get(Engine::CODEX, Path::new("/work/a")).unwrap()[0].name,
            "newer"
        );
        assert!(saved.get(Engine::CLAUDE, Path::new("/work/a")).is_some());
        assert_eq!(
            mine.get(Engine::CODEX, Path::new("/work/a")).unwrap()[0].name,
            "newer",
            "and this window learns it"
        );
    }
}
