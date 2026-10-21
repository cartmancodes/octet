//! Telling apart the same model reached through different providers: Codex's
//! `gpt-5.5` and opencode's `openai/gpt-5.5` are one model, so the lists
//! name the provider in brackets, as `GPT-5.5 (codex)` and
//! `GPT-5.5 (opencode · openai)`.
use octet_core::{Engine, ModelInfo};
use std::collections::{HashMap, HashSet};

/// How a model is reached: its provider and, for opencode's `sub/model`
/// IDs, the part before the last `/` (lowercased).
type Route = (Engine, Option<String>);

/// The ID or selection of `model` split into its route and its base, the
/// part after the last `/` (lowercased) that two providers share.
fn route_and_base(engine: Engine, model: &ModelInfo) -> (Route, String) {
    let id = model.id.as_deref().unwrap_or(&model.selection);
    match id.rsplit_once('/') {
        Some((sub, base)) => ((engine, Some(sub.to_lowercase())), base.to_lowercase()),
        None => ((engine, None), id.to_lowercase()),
    }
}

/// One label per entry: `Some("Name (engine)")` or
/// `Some("Name (engine · sub)")` for a model another entry reaches through a
/// different route, `None` for one that clashes with nothing.
pub(crate) fn labels(entries: &[(Engine, ModelInfo)]) -> Vec<Option<String>> {
    let parsed: Vec<(Route, String)> = entries
        .iter()
        .map(|(engine, model)| route_and_base(*engine, model))
        .collect();
    let mut routes: HashMap<&str, HashSet<&Route>> = HashMap::new();
    for (route, base) in &parsed {
        routes.entry(base).or_default().insert(route);
    }
    entries
        .iter()
        .zip(&parsed)
        .map(|((_, model), (route, base))| {
            if routes.get(base.as_str()).is_none_or(|set| set.len() < 2) {
                return None;
            }
            let name = crate::text::clean(&model.name);
            let display = name
                .rsplit_once('/')
                .map_or(name.as_str(), |(_, rest)| rest);
            Some(match &route.1 {
                Some(sub) => format!("{display} ({} · {sub})", route.0),
                None => format!("{display} ({})", route.0),
            })
        })
        .collect()
}

/// The selection and provider a picker or Tab row names. Reads today's
/// `sel · engine …` rows and the clash rows `sel (engine…)` and
/// `sel · Name (engine…)`, each with the picker's trailing ` ●` allowed.
/// The provider is `None` when the row names none.
pub(crate) fn row_route(item: &str) -> (&str, Option<Engine>) {
    let item = item.strip_suffix(" ●").unwrap_or(item);
    let cut = [item.find(" · "), item.find(" (")]
        .into_iter()
        .flatten()
        .min();
    let Some(cut) = cut else {
        return (item, None);
    };
    let (selection, rest) = item.split_at(cut);
    let engine = match rest.strip_prefix(" · ") {
        Some(rest) => {
            let head = rest.split(" · ").next().unwrap_or(rest);
            Engine::parse(head).or_else(|| bracketed(rest))
        }
        None => bracketed(rest),
    };
    (selection, engine)
}

/// The provider in the last `(engine)` or `(engine · sub)` of `text`.
fn bracketed(text: &str) -> Option<Engine> {
    let inner = text[text.rfind(" (")? + 2..].strip_suffix(')')?;
    Engine::parse(inner.split(" · ").next()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(engine: Engine, id: &str, name: &str) -> (Engine, ModelInfo) {
        (
            engine,
            ModelInfo {
                selection: id.into(),
                id: Some(id.into()),
                name: name.into(),
                description: String::new(),
            },
        )
    }

    #[test]
    fn the_same_model_through_two_providers_is_labelled() {
        let entries = [
            entry(Engine::CODEX, "gpt-5.5", "GPT-5.5"),
            entry(Engine::OPENCODE, "openai/gpt-5.5", "OpenAI/GPT-5.5"),
        ];
        assert_eq!(
            labels(&entries),
            [
                Some("GPT-5.5 (codex)".to_owned()),
                Some("GPT-5.5 (opencode · openai)".to_owned())
            ]
        );
    }

    #[test]
    fn two_opencode_sub_providers_clash() {
        let entries = [
            entry(Engine::OPENCODE, "openai/gpt-5.5", "OpenAI/GPT-5.5"),
            entry(Engine::OPENCODE, "azure/gpt-5.5", "Azure/GPT-5.5"),
        ];
        assert_eq!(
            labels(&entries),
            [
                Some("GPT-5.5 (opencode · openai)".to_owned()),
                Some("GPT-5.5 (opencode · azure)".to_owned())
            ]
        );
    }

    #[test]
    fn different_models_are_not_labelled() {
        let entries = [
            entry(Engine::CODEX, "gpt-5.5", "GPT-5.5"),
            entry(
                Engine::OPENCODE,
                "opencode/glm-5.3-flash-free",
                "OpenCode/GLM 5.3 Flash Free",
            ),
        ];
        assert_eq!(labels(&entries), [None, None]);
    }

    #[test]
    fn case_is_ignored() {
        let entries = [
            entry(Engine::CODEX, "GPT-5.5", "GPT-5.5"),
            entry(Engine::OPENCODE, "openai/gpt-5.5", "OpenAI/GPT-5.5"),
        ];
        assert!(labels(&entries).iter().all(Option::is_some));
    }

    #[test]
    fn rows_route_back_to_their_provider() {
        assert_eq!(
            row_route("openai/gpt-5.5 · GPT-5.5 (opencode · openai)"),
            ("openai/gpt-5.5", Some(Engine::OPENCODE))
        );
        assert_eq!(
            row_route("gpt-5.5 (codex)"),
            ("gpt-5.5", Some(Engine::CODEX))
        );
        assert_eq!(
            row_route("gpt-6-astra · codex · GPT-6 Astra"),
            ("gpt-6-astra", Some(Engine::CODEX))
        );
    }

    #[test]
    fn rows_route_with_the_current_marker_and_without_a_provider() {
        assert_eq!(
            row_route("gpt-5.5 · GPT-5.5 (codex) ●"),
            ("gpt-5.5", Some(Engine::CODEX))
        );
        assert_eq!(
            row_route("gpt-6-astra · codex · GPT-6 Astra ●"),
            ("gpt-6-astra", Some(Engine::CODEX))
        );
        assert_eq!(row_route("opus"), ("opus", None));
    }
}
