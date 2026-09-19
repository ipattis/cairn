//! The four file tools the curator agents get: `read`, `list`, `str_replace`, `create`.
//!
//! No shell, no web. Path rules are code, not config: every tool resolves the real path
//! and refuses anything outside its own area, so there is no bash to route around them.

use std::path::PathBuf;

use crate::model::ToolSchema;
use crate::vault::{Area, Vault};
use crate::Result;

/// A tool run's result as the loop feeds it back to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// File tools scoped to one vault area, plus optional read-only areas.
pub struct FileTools {
    vault: Vault,
    /// The only area this agent may write to.
    write_area: Area,
    /// Areas it may read, e.g. the maintainer reads `raw/` and writes `wiki/`.
    read_areas: Vec<Area>,
    /// Paths (relative to their area) that have already been created this session,
    /// so `create` cannot be used to clobber a file the agent should be editing.
    created: std::sync::Mutex<Vec<PathBuf>>,
}

impl FileTools {
    pub fn new(vault: Vault, write_area: Area, read_areas: Vec<Area>) -> Self {
        let mut read_areas = read_areas;
        if !read_areas.contains(&write_area) {
            read_areas.push(write_area);
        }
        Self {
            vault,
            write_area,
            read_areas,
            created: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn write_area(&self) -> Area {
        self.write_area
    }

    /// Schemas advertised to the model.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        let readable = self
            .read_areas
            .iter()
            .map(|a| format!("{}/", a.dir_name()))
            .collect::<Vec<_>>()
            .join(", ");
        let writable = format!("{}/", self.write_area.dir_name());
        vec![
            ToolSchema {
                name: "list".into(),
                description: format!(
                    "List files under a directory. Readable areas: {readable}. \
                     Paths are relative to the area, e.g. `wiki/patterns`."
                ),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Area-prefixed directory, e.g. `wiki/patterns`. Use the area name alone for its root."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            },
            ToolSchema {
                name: "read".into(),
                description: format!("Read a whole file. Readable areas: {readable}."),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Area-prefixed file path, e.g. `raw/iter-001/fix-test.md`."}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            },
            ToolSchema {
                name: "str_replace".into(),
                description: format!(
                    "Replace an exact string in a file under {writable}. `old_str` must \
                     appear exactly once. This is the only way to edit an existing file."
                ),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old_str": {"type": "string", "description": "Exact text to replace; must be unique in the file."},
                        "new_str": {"type": "string", "description": "Replacement text. Use an empty string to delete."}
                    },
                    "required": ["path", "old_str", "new_str"],
                    "additionalProperties": false
                }),
            },
            ToolSchema {
                name: "create".into(),
                description: format!(
                    "Create a new file under {writable}. Fails if the file already \
                     exists; use str_replace to edit."
                ),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "contents": {"type": "string"}
                    },
                    "required": ["path", "contents"],
                    "additionalProperties": false
                }),
            },
        ]
    }

    /// Runs one tool call. Errors are returned as tool output, not as `Err`: the model
    /// should see and recover from them, and only a broken loop should abort the run.
    pub async fn run(&self, name: &str, args: &serde_json::Value) -> ToolOutput {
        match self.try_run(name, args).await {
            Ok(out) => out,
            Err(e) => ToolOutput::err(format!("error: {e}")),
        }
    }

    async fn try_run(&self, name: &str, args: &serde_json::Value) -> Result<ToolOutput> {
        match name {
            "list" => self.list(str_arg(args, "path")?).await,
            "read" => self.read(str_arg(args, "path")?).await,
            "str_replace" => {
                self.str_replace(
                    str_arg(args, "path")?,
                    str_arg(args, "old_str")?,
                    str_arg(args, "new_str")?,
                )
                .await
            }
            "create" => {
                self.create(str_arg(args, "path")?, str_arg(args, "contents")?)
                    .await
            }
            other => Ok(ToolOutput::err(format!(
                "unknown tool `{other}`; available: list, read, str_replace, create"
            ))),
        }
    }

    /// Splits an area-prefixed path like `wiki/patterns/x.md` into its area and remainder.
    fn split_area(&self, path: &str) -> Result<(Area, String)> {
        let path = path.trim().trim_start_matches("./");
        let (head, rest) = match path.split_once('/') {
            Some((head, rest)) => (head, rest.to_string()),
            None => (path, String::new()),
        };
        let area = match head {
            "wiki" => Area::Wiki,
            "skills" => Area::Skills,
            "raw" => Area::Raw,
            "eval" => Area::Eval,
            other => anyhow::bail!(
                "paths must start with an area name (wiki/, skills/, raw/, eval/); got `{other}`"
            ),
        };
        Ok((area, rest))
    }

    fn check_readable(&self, area: Area) -> Result<()> {
        if !self.read_areas.contains(&area) {
            anyhow::bail!(
                "this role cannot read {}/ — readable areas: {}",
                area.dir_name(),
                self.read_areas
                    .iter()
                    .map(|a| a.dir_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(())
    }

    fn check_writable(&self, area: Area) -> Result<()> {
        if area != self.write_area {
            anyhow::bail!(
                "this role writes only under {}/ — refusing to write {}/",
                self.write_area.dir_name(),
                area.dir_name()
            );
        }
        Ok(())
    }

    async fn list(&self, path: &str) -> Result<ToolOutput> {
        let (area, rest) = self.split_area(path)?;
        self.check_readable(area)?;
        let dir = if rest.is_empty() {
            self.vault.area_root(area)
        } else {
            self.vault.resolve_in(area, &rest)?
        };
        if !dir.is_dir() {
            return Ok(ToolOutput::err(format!("not a directory: {path}")));
        }
        let mut entries = tokio::fs::read_dir(&dir).await?;
        let mut lines = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let suffix = if entry.file_type().await?.is_dir() {
                "/"
            } else {
                ""
            };
            lines.push(format!("{}/{}{}", path.trim_end_matches('/'), name, suffix));
        }
        lines.sort();
        if lines.is_empty() {
            return Ok(ToolOutput::ok(format!("(empty) {path}")));
        }
        Ok(ToolOutput::ok(lines.join("\n")))
    }

    async fn read(&self, path: &str) -> Result<ToolOutput> {
        let (area, rest) = self.split_area(path)?;
        self.check_readable(area)?;
        if rest.is_empty() {
            return Ok(ToolOutput::err(format!("{path} is a directory; use list")));
        }
        let file = self.vault.resolve_in(area, &rest)?;
        match tokio::fs::read_to_string(&file).await {
            Ok(text) => Ok(ToolOutput::ok(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(ToolOutput::err(format!("no such file: {path}")))
            }
            Err(e) => Ok(ToolOutput::err(format!("cannot read {path}: {e}"))),
        }
    }

    async fn str_replace(&self, path: &str, old: &str, new: &str) -> Result<ToolOutput> {
        let (area, rest) = self.split_area(path)?;
        self.check_writable(area)?;
        if rest.is_empty() {
            return Ok(ToolOutput::err("path must name a file".to_string()));
        }
        if old.is_empty() {
            return Ok(ToolOutput::err(
                "old_str must not be empty; use create for new files".to_string(),
            ));
        }
        let file = self.vault.resolve_in(area, &rest)?;
        let text = match tokio::fs::read_to_string(&file).await {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput::err(format!(
                    "no such file: {path}; use create to make it"
                )))
            }
            Err(e) => return Ok(ToolOutput::err(format!("cannot read {path}: {e}"))),
        };
        let hits = text.matches(old).count();
        if hits == 0 {
            return Ok(ToolOutput::err(format!(
                "old_str not found in {path}; read the file and copy the text exactly"
            )));
        }
        if hits > 1 {
            return Ok(ToolOutput::err(format!(
                "old_str appears {hits} times in {path}; include more context to make it unique"
            )));
        }
        tokio::fs::write(&file, text.replacen(old, new, 1)).await?;
        Ok(ToolOutput::ok(format!("edited {path}")))
    }

    async fn create(&self, path: &str, contents: &str) -> Result<ToolOutput> {
        let (area, rest) = self.split_area(path)?;
        self.check_writable(area)?;
        if rest.is_empty() {
            return Ok(ToolOutput::err("path must name a file".to_string()));
        }
        let file = self.vault.resolve_in(area, &rest)?;
        if file.exists() {
            return Ok(ToolOutput::err(format!(
                "{path} already exists; use str_replace to edit it"
            )));
        }
        if let Some(parent) = file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&file, contents).await?;
        self.created.lock().unwrap().push(file);
        Ok(ToolOutput::ok(format!(
            "created {path} ({} bytes)",
            contents.len()
        )))
    }
}

fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required string argument `{key}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn maintainer_tools() -> (tempfile::TempDir, FileTools) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::init(dir.path().join("V")).await.unwrap();
        tokio::fs::create_dir_all(vault.raw_iter_dir(1)).await.unwrap();
        tokio::fs::write(vault.raw_iter_dir(1).join("t1.md"), "trace body")
            .await
            .unwrap();
        let tools = FileTools::new(vault, Area::Wiki, vec![Area::Raw]);
        (dir, tools)
    }

    #[tokio::test]
    async fn advertises_exactly_four_tools() {
        let (_d, tools) = maintainer_tools().await;
        let names: Vec<_> = tools.schemas().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["list", "read", "str_replace", "create"]);
    }

    #[tokio::test]
    async fn maintainer_reads_raw_and_writes_wiki_only() {
        let (_d, tools) = maintainer_tools().await;
        let out = tools
            .run("read", &serde_json::json!({"path": "raw/iter-001/t1.md"}))
            .await;
        assert_eq!(out.content, "trace body");

        let denied = tools
            .run(
                "create",
                &serde_json::json!({"path": "skills/x/SKILL.md", "contents": "x"}),
            )
            .await;
        assert!(denied.is_error);
        assert!(denied.content.contains("writes only under wiki/"), "{denied:?}");

        let allowed = tools
            .run(
                "create",
                &serde_json::json!({"path": "wiki/patterns/loop.md", "contents": "# loop\n"}),
            )
            .await;
        assert!(!allowed.is_error, "{allowed:?}");
    }

    #[tokio::test]
    async fn proposer_cannot_read_the_wiki_it_was_not_given() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::init(dir.path().join("V")).await.unwrap();
        // A proposer configured without raw access must not reach it.
        let tools = FileTools::new(vault, Area::Skills, vec![Area::Wiki]);
        let out = tools
            .run("list", &serde_json::json!({"path": "raw"}))
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("cannot read raw/"), "{out:?}");
    }

    #[tokio::test]
    async fn traversal_and_absolute_paths_are_refused() {
        let (_d, tools) = maintainer_tools().await;
        for path in ["wiki/../skills/x.md", "/etc/passwd", "etc/passwd"] {
            let out = tools
                .run("create", &serde_json::json!({"path": path, "contents": "x"}))
                .await;
            assert!(out.is_error, "{path} should be refused: {out:?}");
        }
    }

    #[tokio::test]
    async fn str_replace_demands_a_unique_match() {
        let (_d, tools) = maintainer_tools().await;
        tools
            .run(
                "create",
                &serde_json::json!({"path": "wiki/logs2.md", "contents": "a\nrepeat\nrepeat\n"}),
            )
            .await;

        let ambiguous = tools
            .run(
                "str_replace",
                &serde_json::json!({"path": "wiki/logs2.md", "old_str": "repeat", "new_str": "x"}),
            )
            .await;
        assert!(ambiguous.is_error);
        assert!(ambiguous.content.contains("appears 2 times"), "{ambiguous:?}");

        let missing = tools
            .run(
                "str_replace",
                &serde_json::json!({"path": "wiki/logs2.md", "old_str": "nope", "new_str": "x"}),
            )
            .await;
        assert!(missing.content.contains("not found"), "{missing:?}");

        let ok = tools
            .run(
                "str_replace",
                &serde_json::json!({"path": "wiki/logs2.md", "old_str": "a\n", "new_str": "b\n"}),
            )
            .await;
        assert!(!ok.is_error, "{ok:?}");
    }

    #[tokio::test]
    async fn create_refuses_to_clobber_and_edits_need_str_replace() {
        let (_d, tools) = maintainer_tools().await;
        let args = serde_json::json!({"path": "wiki/index.md", "contents": "clobbered"});
        let out = tools.run("create", &args).await;
        assert!(out.is_error);
        assert!(out.content.contains("already exists"), "{out:?}");
    }

    #[tokio::test]
    async fn bad_arguments_and_unknown_tools_come_back_as_tool_errors() {
        let (_d, tools) = maintainer_tools().await;
        let out = tools.run("read", &serde_json::json!({})).await;
        assert!(out.is_error);
        assert!(out.content.contains("missing required string argument"));

        let out = tools.run("bash", &serde_json::json!({"cmd": "ls"})).await;
        assert!(out.is_error);
        assert!(out.content.contains("unknown tool"));
    }

    #[tokio::test]
    async fn list_sorts_and_marks_directories() {
        let (_d, tools) = maintainer_tools().await;
        let out = tools.run("list", &serde_json::json!({"path": "wiki"})).await;
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("wiki/patterns/"), "{out:?}");
        assert!(out.content.contains("wiki/index.md"), "{out:?}");
    }
}
