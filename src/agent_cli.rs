//! Agent CLI module — clap derive subcommands under `agend-terminal agent`.

use clap::Subcommand;
use serde_json::{json, Value};
use std::path::Path;

// ---------------------------------------------------------------------------
// Output helper
// ---------------------------------------------------------------------------

fn output(value: Value) {
    let json = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
    println!("{json}");
    if value.get("error").is_some() {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Top-level agent command
// ---------------------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    // -- High-frequency (flat) ------------------------------------------------
    /// Send a message to another agent.
    Send { target: String, message: String },
    /// Delegate a task to another agent.
    Delegate {
        target: String,
        task: String,
        #[arg(long)]
        criteria: Option<String>,
        #[arg(long)]
        context: Option<String>,
    },
    /// Report a result to another agent.
    Report {
        target: String,
        summary: String,
        #[arg(long)]
        correlation_id: Option<String>,
        #[arg(long)]
        artifacts: Option<String>,
    },
    /// Ask another agent a question.
    Ask {
        target: String,
        question: String,
        #[arg(long)]
        context: Option<String>,
    },
    /// Broadcast a message to a team or all agents.
    Broadcast {
        message: String,
        #[arg(long)]
        team: Option<String>,
        #[arg(long)]
        targets: Option<String>,
    },
    /// Drain the inbox.
    Inbox,
    /// Reply in the channel.
    Reply { text: String },
    /// List instances.
    #[command(alias = "ls")]
    List,

    // -- Instance management --------------------------------------------------
    /// Spawn a new agent instance.
    Spawn {
        name: String,
        #[arg(long)]
        backend: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, alias = "dir")]
        working_directory: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        role: Option<String>,
    },
    /// Start an existing agent instance.
    Start { name: String },
    /// Delete an agent instance.
    Delete { name: String },
    /// Describe an agent instance.
    Describe { name: String },
    /// Replace an agent instance (kill + respawn with handover).
    Replace {
        name: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Set display name for the current instance.
    Rename { display_name: String },
    /// Set description for the current instance.
    SetDescription { description: String },

    // -- Grouped CRUD ---------------------------------------------------------
    /// Task board operations.
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Decision log operations.
    Decision {
        #[command(subcommand)]
        command: DecisionCommand,
    },
    /// Team management.
    Team {
        #[command(subcommand)]
        command: TeamCommand,
    },
    /// Schedule management.
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommand,
    },
    /// Deployment management.
    Deploy {
        #[command(subcommand)]
        command: DeployCommand,
    },
    /// Repository operations.
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// CI watch operations.
    Ci {
        #[command(subcommand)]
        command: CiCommand,
    },
    /// Channel operations.
    Channel {
        #[command(subcommand)]
        command: ChannelCommand,
    },
}

// ---------------------------------------------------------------------------
// Sub-enums
// ---------------------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// Create a new task.
    Create {
        title: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
    },
    /// List tasks.
    List {
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        status: Option<String>,
    },
    /// Claim a task.
    Claim { id: String },
    /// Mark a task as done.
    Done {
        id: String,
        #[arg(long)]
        result: Option<String>,
    },
    /// Update a task.
    Update {
        id: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum DecisionCommand {
    /// Post a new decision.
    Post {
        title: String,
        content: String,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long)]
        tags: Option<String>,
    },
    /// List decisions.
    List {
        #[arg(long)]
        archived: bool,
        #[arg(long)]
        tags: Option<String>,
    },
    /// Update a decision.
    Update {
        id: String,
        #[arg(long)]
        content: Option<String>,
        #[arg(long)]
        tags: Option<String>,
        #[arg(long)]
        archive: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum TeamCommand {
    /// Create a team.
    Create { name: String, members: Vec<String> },
    /// List teams.
    List,
    /// Delete a team.
    Delete { name: String },
    /// Update a team.
    Update {
        name: String,
        #[arg(long)]
        add: Option<String>,
        #[arg(long)]
        remove: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ScheduleCommand {
    /// Create a schedule.
    Create {
        cron: String,
        message: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        tz: Option<String>,
    },
    /// List schedules.
    List {
        #[arg(long)]
        target: Option<String>,
    },
    /// Update a schedule.
    Update {
        id: String,
        #[arg(long)]
        cron: Option<String>,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        enabled: Option<bool>,
    },
    /// Delete a schedule.
    Delete { id: String },
}

#[derive(Debug, Subcommand)]
pub enum DeployCommand {
    /// Run a deployment.
    Run {
        template: String,
        directory: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        branch: Option<String>,
    },
    /// Tear down a deployment.
    Teardown { name: String },
    /// List deployments.
    List,
}

#[derive(Debug, Subcommand)]
pub enum RepoCommand {
    /// Checkout a repo as a worktree.
    Checkout {
        source: String,
        #[arg(long)]
        branch: Option<String>,
    },
    /// Release a worktree.
    Release { path: String },
}

#[derive(Debug, Subcommand)]
pub enum CiCommand {
    /// Watch a repo's CI.
    Watch {
        repo: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        interval: Option<u64>,
    },
    /// Stop watching a repo's CI.
    Unwatch { repo: String },
}

#[derive(Debug, Subcommand)]
pub enum ChannelCommand {
    /// Reply in the channel.
    Reply { text: String },
    /// React to a message with an emoji.
    React {
        emoji: String,
        #[arg(long)]
        message_id: Option<String>,
    },
    /// Edit a channel message.
    Edit { message_id: String, text: String },
    /// Download a file attachment.
    Download { file_id: String },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Split a comma-separated string into a JSON array of strings.
fn csv_to_json_array(s: &str) -> Value {
    let items: Vec<Value> = s
        .split(',')
        .map(|t| Value::String(t.trim().to_string()))
        .filter(|v| v.as_str() != Some(""))
        .collect();
    Value::Array(items)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(home: &Path, command: AgentCommand) {
    let me = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();

    match command {
        // -- Communication ----------------------------------------------------
        AgentCommand::Send { target, message } => {
            output(crate::ops::send_message(home, &me, &target, &message, None));
        }
        AgentCommand::Delegate {
            target,
            task,
            criteria,
            context,
        } => {
            output(crate::ops::delegate_task(
                home,
                &me,
                &target,
                &task,
                criteria.as_deref(),
                context.as_deref(),
            ));
        }
        AgentCommand::Report {
            target,
            summary,
            correlation_id,
            artifacts,
        } => {
            output(crate::ops::report_result(
                home,
                &me,
                &target,
                &summary,
                correlation_id.as_deref(),
                artifacts.as_deref(),
            ));
        }
        AgentCommand::Ask {
            target,
            question,
            context,
        } => {
            output(crate::ops::request_information(
                home,
                &me,
                &target,
                &question,
                context.as_deref(),
            ));
        }
        AgentCommand::Broadcast {
            message,
            team,
            targets,
        } => {
            let target_list: Option<Vec<String>> = targets
                .as_deref()
                .map(|s| s.split(',').map(|t| t.trim().to_string()).collect());
            output(crate::ops::broadcast(
                home,
                &me,
                &message,
                team.as_deref(),
                target_list.as_deref(),
            ));
        }
        AgentCommand::Inbox => {
            output(crate::ops::drain_inbox(home, &me));
        }
        AgentCommand::Reply { text } => {
            output(crate::ops::reply(home, &me, &text));
        }
        AgentCommand::List => {
            output(crate::ops::list_instances(home));
        }

        // -- Instance management ----------------------------------------------
        AgentCommand::Spawn {
            name,
            backend,
            model,
            working_directory,
            branch,
            task,
            role,
        } => {
            let mut args = json!({"name": name});
            if let Some(b) = backend {
                args["backend"] = json!(b);
            }
            if let Some(m) = model {
                args["model"] = json!(m);
            }
            if let Some(wd) = working_directory {
                args["working_directory"] = json!(wd);
            }
            if let Some(br) = branch {
                args["branch"] = json!(br);
            }
            if let Some(t) = task {
                args["task"] = json!(t);
            }
            if let Some(r) = role {
                args["role"] = json!(r);
            }
            output(crate::ops::create_instance(home, &args));
        }
        AgentCommand::Start { name } => {
            output(crate::ops::start_instance(home, &json!({"name": name})));
        }
        AgentCommand::Delete { name } => {
            output(crate::ops::delete_instance(home, &json!({"name": name})));
        }
        AgentCommand::Describe { name } => {
            output(crate::ops::describe_instance(home, &name));
        }
        AgentCommand::Replace { name, reason } => {
            let reason = reason.as_deref().unwrap_or("manual replacement");
            output(crate::ops::replace_instance(home, &name, reason));
        }
        AgentCommand::Rename { display_name } => {
            output(crate::ops::set_display_name(home, &me, &display_name));
        }
        AgentCommand::SetDescription { description } => {
            output(crate::ops::set_description(home, &me, &description));
        }

        // -- Task board -------------------------------------------------------
        AgentCommand::Task { command } => match command {
            TaskCommand::Create {
                title,
                description,
                priority,
                assignee,
            } => {
                output(crate::tasks::handle(
                    home,
                    &me,
                    &json!({
                        "action": "create",
                        "title": title,
                        "description": description.unwrap_or_default(),
                        "priority": priority.unwrap_or_else(|| "normal".into()),
                        "assignee": assignee,
                    }),
                ));
            }
            TaskCommand::List { assignee, status } => {
                output(crate::tasks::handle(
                    home,
                    &me,
                    &json!({
                        "action": "list",
                        "assignee": assignee,
                        "status": status,
                    }),
                ));
            }
            TaskCommand::Claim { id } => {
                output(crate::tasks::handle(
                    home,
                    &me,
                    &json!({"action": "claim", "id": id}),
                ));
            }
            TaskCommand::Done { id, result } => {
                output(crate::tasks::handle(
                    home,
                    &me,
                    &json!({"action": "done", "id": id, "result": result}),
                ));
            }
            TaskCommand::Update {
                id,
                status,
                priority,
                assignee,
            } => {
                output(crate::tasks::handle(
                    home,
                    &me,
                    &json!({
                        "action": "update",
                        "id": id,
                        "status": status,
                        "priority": priority,
                        "assignee": assignee,
                    }),
                ));
            }
        },

        // -- Decisions --------------------------------------------------------
        AgentCommand::Decision { command } => match command {
            DecisionCommand::Post {
                title,
                content,
                scope,
                tags,
            } => {
                let mut args = json!({
                    "title": title,
                    "content": content,
                    "scope": scope.unwrap_or_else(|| "project".into()),
                });
                if let Some(tags) = tags {
                    args["tags"] = csv_to_json_array(&tags);
                }
                output(crate::decisions::post(home, &me, &args));
            }
            DecisionCommand::List { archived, tags } => {
                let mut args = json!({"archived": archived});
                if let Some(tags) = tags {
                    args["tags"] = csv_to_json_array(&tags);
                }
                output(crate::decisions::list(home, &args));
            }
            DecisionCommand::Update {
                id,
                content,
                tags,
                archive,
            } => {
                let mut args = json!({"id": id, "archive": archive});
                if let Some(content) = content {
                    args["content"] = json!(content);
                }
                if let Some(tags) = tags {
                    args["tags"] = csv_to_json_array(&tags);
                }
                output(crate::decisions::update(home, &args));
            }
        },

        // -- Teams ------------------------------------------------------------
        AgentCommand::Team { command } => match command {
            TeamCommand::Create { name, members } => {
                output(crate::teams::create(
                    home,
                    &json!({"name": name, "members": members}),
                ));
            }
            TeamCommand::List => {
                output(crate::teams::list(home));
            }
            TeamCommand::Delete { name } => {
                output(crate::teams::delete(home, &json!({"name": name})));
            }
            TeamCommand::Update { name, add, remove } => {
                let mut args = json!({"name": name});
                if let Some(add) = add {
                    args["add"] = json!(add);
                }
                if let Some(remove) = remove {
                    args["remove"] = json!(remove);
                }
                output(crate::teams::update(home, &args));
            }
        },

        // -- Schedules --------------------------------------------------------
        AgentCommand::Schedule { command } => match command {
            ScheduleCommand::Create {
                cron,
                message,
                target,
                label,
                tz,
            } => {
                let mut args = json!({"cron": cron, "message": message});
                if let Some(target) = target {
                    args["target"] = json!(target);
                }
                if let Some(label) = label {
                    args["label"] = json!(label);
                }
                if let Some(tz) = tz {
                    args["timezone"] = json!(tz);
                }
                output(crate::schedules::create(home, &me, &args));
            }
            ScheduleCommand::List { target } => {
                let mut args = json!({});
                if let Some(target) = target {
                    args["target"] = json!(target);
                }
                output(crate::schedules::list(home, &args));
            }
            ScheduleCommand::Update {
                id,
                cron,
                message,
                enabled,
            } => {
                let mut args = json!({"id": id});
                if let Some(cron) = cron {
                    args["cron"] = json!(cron);
                }
                if let Some(message) = message {
                    args["message"] = json!(message);
                }
                if let Some(enabled) = enabled {
                    args["enabled"] = json!(enabled);
                }
                output(crate::schedules::update(home, &args));
            }
            ScheduleCommand::Delete { id } => {
                output(crate::schedules::delete(home, &json!({"id": id})));
            }
        },

        // -- Deployments ------------------------------------------------------
        AgentCommand::Deploy { command } => match command {
            DeployCommand::Run {
                template,
                directory,
                name,
                branch,
            } => {
                let mut args = json!({"template": template, "directory": directory});
                if let Some(name) = name {
                    args["name"] = json!(name);
                }
                if let Some(branch) = branch {
                    args["branch"] = json!(branch);
                }
                output(crate::deployments::deploy(home, &me, &args));
            }
            DeployCommand::Teardown { name } => {
                output(crate::deployments::teardown(home, &json!({"name": name})));
            }
            DeployCommand::List => {
                output(crate::deployments::list(home));
            }
        },

        // -- Repo -------------------------------------------------------------
        AgentCommand::Repo { command } => match command {
            RepoCommand::Checkout { source, branch } => {
                let branch = branch.as_deref().unwrap_or("main");
                output(crate::ops::checkout_repo(home, &me, &source, branch));
            }
            RepoCommand::Release { path } => {
                output(crate::ops::release_repo(&path));
            }
        },

        // -- CI ---------------------------------------------------------------
        AgentCommand::Ci { command } => match command {
            CiCommand::Watch {
                repo,
                branch,
                interval,
            } => {
                let branch = branch.as_deref().unwrap_or("main");
                let interval = interval.unwrap_or(300);
                output(crate::ops::watch_ci(home, &me, &repo, branch, interval));
            }
            CiCommand::Unwatch { repo } => {
                output(crate::ops::unwatch_ci(home, &repo));
            }
        },

        // -- Channel ----------------------------------------------------------
        AgentCommand::Channel { command } => match command {
            ChannelCommand::Reply { text } => {
                output(crate::ops::reply(home, &me, &text));
            }
            ChannelCommand::React { emoji, message_id } => {
                output(crate::ops::react(&me, &emoji, message_id.as_deref()));
            }
            ChannelCommand::Edit { message_id, text } => {
                output(crate::ops::edit_message(&me, &message_id, &text));
            }
            ChannelCommand::Download { file_id } => {
                output(crate::ops::download_attachment(&me, &file_id));
            }
        },
    }
}
