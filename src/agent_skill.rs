// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Fabian Schmieder

//! The agent skill devserial ships, so an AI tool knows when and how to use it
//! (#124).
//!
//! The guidance is written by hand. The references are generated from this
//! build: its command tree, its MCP tools and its HTTP routes. A command, tool
//! or route that the binary has is therefore in the skill, and one it lacks is
//! not.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent_skill_installer::{
    Action, Approval, Assessment, Catalog, Consent, Done, Environment, Found, Installer, Outcome,
    PlanError, Problem, Program, Skill, SkillError, Step, Why,
};

/// The skill's name, which is also its directory in every skills folder.
pub const NAME: &str = "devserial";

/// What the skill is for, which an AI tool reads to decide whether to load it.
const DESCRIPTION: &str = "Read from and write to serial devices (UART, USB serial, \
microcontroller consoles, bootloaders, ESP32) through devserial, which keeps a \
searchable capture of everything a port sent. Use it whenever a task involves a \
serial port, a board's console log or flashing an ESP chip, instead of screen, \
minicom, picocom or pyserial.";

/// The hand-written guidance.
const BODY: &str = include_str!("../resources/skill/body.md");

/// The skill as this build would install it.
///
/// # Errors
/// Returns an error only if the skill's own content breaks the specification,
/// which a test rules out.
pub fn skill() -> Result<Skill, SkillError> {
    let skill = Skill::new(NAME, DESCRIPTION, BODY)?
        .with_field("license", "GPL-3.0-or-later")?
        .with_field(
            "compatibility",
            "Needs the devserial binary on the PATH, or its MCP server connected.",
        )?
        .with_file("references/cli.md", cli_reference())?
        .with_file("references/mcp.md", mcp_reference())?;
    #[cfg(feature = "rest")]
    let skill = skill.with_file("references/http.md", http_reference())?;
    Ok(skill)
}

/// Everything the installer needs on this machine: the skill, where the tools
/// look for skills, and where the user's answers are kept.
pub struct Setup {
    catalog: Catalog,
    env: Environment,
    skill: Skill,
    program: Program,
    consent_file: PathBuf,
}

impl Setup {
    /// The setup for the user running devserial.
    ///
    /// # Errors
    /// Returns an error when there is no home directory to look in.
    pub fn for_this_user() -> Result<Self, String> {
        let env = Environment::from_process().ok_or("no home directory to look for AI tools in")?;
        Self::new(env, crate::paths::default_data_dir().join("skill-consent"))
    }

    /// The setup for a given home and consent file, which a test points at a
    /// temporary directory.
    ///
    /// # Errors
    /// Returns an error when the skill or the version is malformed, which a
    /// test rules out.
    pub fn new(env: Environment, consent_file: PathBuf) -> Result<Self, String> {
        Ok(Self {
            catalog: Catalog::builtin(),
            env,
            skill: skill().map_err(|e| e.to_string())?,
            program: Program::new(NAME, env!("CARGO_PKG_VERSION")).map_err(|e| e.to_string())?,
            consent_file,
        })
    }

    /// The tools and folders this setup knows.
    #[must_use]
    pub const fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The installer for this setup.
    #[must_use]
    pub const fn installer(&self) -> Installer<'_> {
        Installer::new(&self.catalog, &self.env, &self.skill, &self.program)
    }

    /// The user's answers so far.
    ///
    /// # Errors
    /// Returns an error when the file exists but cannot be read.
    pub fn consent(&self) -> std::io::Result<Consent> {
        Consent::load(&self.consent_file)
    }

    /// What would happen, place by place.
    ///
    /// # Errors
    /// Returns an error when the answers cannot be read or no plan can be made.
    pub fn assess(&self) -> Result<Assessment, String> {
        let consent = self.consent().map_err(|e| e.to_string())?;
        self.installer()
            .assess(&consent)
            .map_err(|e: PlanError| e.to_string())
    }

    /// Install into `places`, and bring every unchanged older copy up to date
    /// on the way. A place the user declined earlier counts as agreed now.
    ///
    /// # Errors
    /// Returns an error when the answers cannot be read or written, or no plan
    /// can be made.
    pub fn install(&self, places: &[PathBuf]) -> Result<Vec<Outcome>, String> {
        let mut consent = self.consent().map_err(|e| e.to_string())?;
        for place in places {
            consent.withdraw(place);
        }
        consent.save().map_err(|e| e.to_string())?;
        let assessment = self.assess()?;
        let approval = Approval {
            install: places.to_vec(),
            overwrite: Vec::new(),
        };
        Ok(self.installer().apply(&assessment, &approval))
    }

    /// Remember that the user does not want the skill in `places`.
    ///
    /// # Errors
    /// Returns an error when the answers cannot be read or written.
    pub fn decline(&self, places: &[PathBuf]) -> Result<(), String> {
        let mut consent = self.consent().map_err(|e| e.to_string())?;
        for place in places {
            consent.decline(place).map_err(|e| e.to_string())?;
        }
        consent.save().map_err(|e| e.to_string())
    }

    /// Bring every copy devserial installed and nobody changed up to this
    /// version. Installs nothing new and asks nothing.
    ///
    /// # Errors
    /// Returns an error when no plan can be made.
    pub fn update(&self) -> Result<Vec<Outcome>, String> {
        self.installer().update().map_err(|e| e.to_string())
    }

    /// Remove every copy devserial installed that nobody changed.
    ///
    /// # Errors
    /// Returns an error when no plan can be made.
    pub fn remove(&self) -> Result<Vec<Outcome>, String> {
        self.installer().remove().map_err(|e| e.to_string())
    }
}

/// Run [`Setup::update`] for this user in the background (#124, A3).
///
/// For the long-running surfaces: the GUI, the TUI, the daemon and the MCP
/// server. Nothing goes to stdout or stderr, which belong to the MCP protocol
/// and to the terminal monitor; what happened goes to the log.
pub fn update_in_background() {
    let spawned = std::thread::Builder::new()
        .name("devserial-skill".to_string())
        .spawn(|| match Setup::for_this_user().and_then(|setup| setup.update()) {
            Ok(outcomes) => {
                for done in outcomes {
                    tracing::info!(place = %done.place.display(), done = %done.done, "agent skill");
                }
            }
            Err(e) => tracing::debug!(error = %e, "agent skill not updated"),
        });
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "agent skill update not started");
    }
}

/// The places waiting for the user's answer.
#[must_use]
pub fn undecided(assessment: &Assessment) -> Vec<PathBuf> {
    assessment
        .awaiting_approval()
        .map(|step| step.place.clone())
        .collect()
}

/// The places an explicit install goes to: the undecided ones and those the
/// user declined earlier, since asking for it outright overrides a no.
#[must_use]
pub fn installable(assessment: &Assessment) -> Vec<&Step> {
    assessment
        .steps
        .iter()
        .filter(|step| {
            matches!(
                step.action,
                Action::Install | Action::Nothing(Why::Declined)
            )
        })
        .collect()
}

/// A place for people to read, with the home directory as `~`.
#[must_use]
pub fn shown(place: &Path) -> String {
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        return place.display().to_string();
    };
    let home = PathBuf::from(home);
    // Places come with every symlink resolved, so the home has to be too.
    let resolved = std::fs::canonicalize(&home).unwrap_or_else(|_| home.clone());
    [home, resolved]
        .iter()
        .find_map(|home| place.strip_prefix(home).ok())
        .map_or_else(
            || place.display().to_string(),
            |rest| format!("~/{}", rest.display()),
        )
}

/// One place, as a line or two for people: where, for which tools, and what
/// is there or would happen.
#[must_use]
pub fn describe(step: &Step, catalog: &Catalog) -> String {
    let mut line = shown(&step.place);
    if !step.tools.is_empty() {
        let names: Vec<&str> = step
            .tools
            .iter()
            .map(|id| {
                catalog
                    .tools()
                    .iter()
                    .find(|tool| &tool.id == id)
                    .map_or(id.as_str(), |tool| tool.name.as_str())
            })
            .collect();
        let _ = write!(line, " (read by {})", names.join(", "));
    }
    let state = match &step.action {
        Action::Install => "not installed, waiting for your consent".to_string(),
        Action::Update { from } => format!("version {from}, will be updated"),
        Action::Nothing(Why::Declined) => "declined".to_string(),
        Action::Nothing(Why::Current) => "installed and up to date".to_string(),
        Action::Nothing(Why::Newer(version)) => {
            format!("version {version}, newer than this devserial; left alone")
        }
        Action::Nothing(Why::NotPlanned) => match &step.found {
            Found::Absent => "not installed".to_string(),
            _ => "installed".to_string(),
        },
        Action::Report(Problem::Modified) => "changed by hand; left alone".to_string(),
        Action::Report(Problem::Foreign(why)) => {
            format!("something else named {NAME} is there ({why}); left alone")
        }
    };
    let _ = write!(line, "\n    {state}");
    if let Some(root) = &step.git_root {
        let _ = write!(line, "\n    inside the git working tree {}", shown(root));
    }
    line
}

/// What happened in one place, for people.
#[must_use]
pub fn outcome(outcome: &Outcome) -> String {
    let what = match &outcome.done {
        Done::Installed => "installed".to_string(),
        Done::Updated { from } => format!("updated from {from}"),
        Done::Overwritten => "replaced".to_string(),
        Done::Removed => "removed".to_string(),
        Done::Changed => "left alone: it changed while devserial was looking".to_string(),
        Done::Failed(why) => format!("failed: {why}"),
    };
    format!("{}: {what}", shown(&outcome.place))
}

/// Every command and option, from the command tree clap parses.
#[must_use]
pub fn cli_reference() -> String {
    use clap::CommandFactory as _;
    let mut root = crate::cli::Cli::command();
    // Fills in what the derive leaves implicit, such as which options take a
    // value.
    root.build();
    let mut out = String::from("# devserial command line\n\nGenerated from the build.\n\n");
    let globals: Vec<_> = root
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .collect();
    if !globals.is_empty() {
        out.push_str("## Options of every command\n\n");
        for arg in globals {
            option_line(&mut out, arg);
        }
        out.push('\n');
    }
    for sub in root.get_subcommands() {
        command_section(&mut out, sub, "devserial");
    }
    out
}

fn command_section(out: &mut String, cmd: &clap::Command, parent: &str) {
    if cmd.is_hide_set() {
        return;
    }
    let path = format!("{parent} {}", cmd.get_name());
    let _ = writeln!(out, "## {path}\n");
    if let Some(about) = cmd.get_long_about().or_else(|| cmd.get_about()) {
        let _ = writeln!(out, "{about}\n");
    }
    let positionals: Vec<_> = cmd
        .get_positionals()
        .filter(|arg| !arg.is_hide_set())
        .collect();
    let usage: String = positionals
        .iter()
        .map(|arg| {
            let name = arg.get_id().as_str().to_uppercase();
            if arg.is_required_set() {
                format!(" <{name}>")
            } else {
                format!(" [{name}]")
            }
        })
        .collect();
    let _ = writeln!(out, "`{path}{usage}`\n");
    for arg in &positionals {
        if let Some(help) = arg.get_help() {
            let _ = writeln!(out, "- `{}`: {help}", arg.get_id().as_str().to_uppercase());
        }
    }
    for arg in cmd.get_arguments() {
        if arg.is_positional() || arg.is_global_set() || arg.is_hide_set() {
            continue;
        }
        if matches!(arg.get_id().as_str(), "help" | "version") {
            continue;
        }
        option_line(out, arg);
    }
    out.push('\n');
    for sub in cmd.get_subcommands() {
        command_section(out, sub, &path);
    }
}

fn option_line(out: &mut String, arg: &clap::Arg) {
    let mut names = Vec::new();
    if let Some(short) = arg.get_short() {
        names.push(format!("-{short}"));
    }
    if let Some(long) = arg.get_long() {
        names.push(format!("--{long}"));
    }
    if names.is_empty() {
        return;
    }
    let takes_value = arg.get_num_args().is_some_and(|n| n.takes_values())
        && !matches!(
            arg.get_action(),
            clap::ArgAction::SetTrue | clap::ArgAction::SetFalse | clap::ArgAction::Count
        );
    let value = if takes_value {
        let name = arg
            .get_value_names()
            .and_then(|names| names.first())
            .map_or_else(|| arg.get_id().as_str().to_uppercase(), ToString::to_string);
        format!(" <{name}>")
    } else {
        String::new()
    };
    let _ = write!(out, "- `{}{value}`", names.join(", "));
    if let Some(help) = arg.get_help() {
        let help = help.to_string();
        let _ = write!(out, ": {}", sentence(&help));
    }
    let possible: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| format!("`{}`", value.get_name()))
        .collect();
    if takes_value && !possible.is_empty() {
        let _ = write!(out, " One of {}.", possible.join(", "));
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    if takes_value && !defaults.is_empty() {
        let _ = write!(out, " Default `{}`.", defaults.join(","));
    }
    out.push('\n');
}

/// A help text as a sentence, so what follows it reads as the next one.
fn sentence(text: &str) -> String {
    let text = text.trim_end();
    if text.ends_with(['.', '!', '?', ')']) {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

/// Every MCP tool, with its parameters, from the tool router.
#[must_use]
pub fn mcp_reference() -> String {
    let mut out = String::from(
        "# devserial MCP tools\n\nGenerated from the build. Started with `devserial mcp`.\n\n",
    );
    let mut tools = crate::server::DevSerialServer::tools();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    for tool in tools {
        let _ = writeln!(out, "## {}\n", tool.name);
        if let Some(description) = &tool.description {
            let _ = writeln!(out, "{description}\n");
        }
        let required: Vec<&str> = tool
            .input_schema
            .get("required")
            .and_then(|value| value.as_array())
            .map(|names| names.iter().filter_map(|name| name.as_str()).collect())
            .unwrap_or_default();
        if let Some(properties) = tool
            .input_schema
            .get("properties")
            .and_then(|value| value.as_object())
        {
            for (name, schema) in properties {
                let needed = if required.contains(&name.as_str()) {
                    " (required)"
                } else {
                    ""
                };
                let what = schema
                    .get("description")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                let _ = writeln!(out, "- `{name}`{needed}: {what}");
            }
            out.push('\n');
        }
    }
    out
}

/// The HTTP routes, from the router's own list.
#[cfg(feature = "rest")]
#[must_use]
pub fn http_reference() -> String {
    let mut out = String::from(
        "# devserial HTTP interface\n\nGenerated from the build. Off by default; \
         `devserial rest` shows and changes it. `GET /v1/openapi.json` describes \
         every route with its parameters.\n\n",
    );
    for route in crate::rest::ROUTES {
        let _ = writeln!(out, "- `{route}`");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The skill is valid under the specification as this build makes it.
    #[test]
    fn the_skill_is_valid() {
        let skill = skill().expect("a valid skill");
        assert_eq!(skill.name(), NAME);
    }

    /// Every command of the binary is in the reference, nested ones too.
    #[test]
    fn every_command_is_in_the_reference() {
        use clap::CommandFactory as _;
        fn walk(cmd: &clap::Command, parent: &str, text: &str) {
            for sub in cmd.get_subcommands().filter(|sub| !sub.is_hide_set()) {
                let path = format!("{parent} {}", sub.get_name());
                assert!(text.contains(&format!("## {path}\n")), "{path} is missing");
                for arg in sub.get_arguments().filter(|a| !a.is_global_set()) {
                    if let Some(long) = arg.get_long()
                        && !matches!(long, "help" | "version")
                    {
                        assert!(text.contains(&format!("--{long}")), "{path} --{long}");
                    }
                }
                walk(sub, &path, text);
            }
        }
        let text = cli_reference();
        walk(&crate::cli::Cli::command(), "devserial", &text);
    }

    /// Every MCP tool is in the reference, and all of them carry the prefix
    /// the guidance tells the agent to look for.
    #[test]
    fn every_tool_is_in_the_reference() {
        let text = mcp_reference();
        let tools = crate::server::DevSerialServer::tools();
        assert!(!tools.is_empty(), "no tools listed");
        for tool in tools {
            assert!(
                text.contains(&format!("## {}\n", tool.name)),
                "{} is missing",
                tool.name
            );
            assert!(tool.name.starts_with("serial_"), "{}", tool.name);
        }
    }

    /// Every HTTP route is in the reference.
    #[cfg(feature = "rest")]
    #[test]
    fn every_route_is_in_the_reference() {
        let text = http_reference();
        for route in crate::rest::ROUTES {
            assert!(text.contains(&format!("`{route}`")), "{route} is missing");
        }
    }

    /// The commands the guidance names exist, with the options it uses.
    #[test]
    fn the_guidance_names_real_commands() {
        let text = cli_reference();
        for needed in [
            "## devserial list\n",
            "## devserial open\n",
            "## devserial read\n",
            "## devserial search\n",
            "## devserial write\n",
            "## devserial signal\n",
            "## devserial macro\n",
            "## devserial clear\n",
            "## devserial close\n",
            "--tail",
            "--after",
            "--since",
            "--wait-ms",
            "--json",
            "--newline",
            "--hex",
            "--dtr",
            "--archive",
            "--mode",
        ] {
            assert!(text.contains(needed), "{needed:?}");
        }
        assert!(
            crate::config::BUILTIN_MACRO_NAMES.contains(&"reset"),
            "the guidance names the reset macro"
        );
    }
}
