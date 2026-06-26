use super::InstanceYamlEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldClass {
    DaemonManaged,
    OperatorHandEdit,
}

pub fn instance_field_class(field: &str) -> FieldClass {
    match field {
        "id" | "topic_id" | "git_branch" | "source_repo" => FieldClass::DaemonManaged,
        _ => FieldClass::OperatorHandEdit,
    }
}

#[derive(Debug, Clone)]
pub struct FieldConflict {
    pub instance: String,
    pub field: String,
    pub operator_value: String,
    pub daemon_value: String,
}

impl std::fmt::Display for FieldConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fleet.yaml merge conflict — instance `{}` field `{}`: operator has `{}`, daemon proposed `{}`. \
             Hand-edit fleet.yaml to resolve, OR delete the field to accept daemon's value.",
            self.instance, self.field, self.operator_value, self.daemon_value
        )
    }
}

pub fn merge_instance_into_existing(
    name: &str,
    existing: &mut serde_yaml_ng::Mapping,
    config: &InstanceYamlEntry,
) -> Result<(), FieldConflict> {
    use serde_yaml_ng::Value;

    for (field, value) in [
        ("backend", &config.backend),
        ("working_directory", &config.working_directory),
        ("role", &config.role),
        ("instructions", &config.instructions),
        ("source_repo", &config.source_repo),
        ("repo", &config.repo),
        ("github_login", &config.github_login),
        ("model", &config.model),
        ("model_tier", &config.model_tier),
        ("ready_pattern", &config.ready_pattern),
        ("command", &config.command),
        ("topic_binding_mode", &config.topic_binding_mode),
    ] {
        merge_string_field(name, existing, field, value)?;
    }

    merge_typed_field(
        name,
        existing,
        "args",
        config
            .args
            .as_ref()
            .map(|args| Value::Sequence(args.iter().map(|s| Value::String(s.clone())).collect())),
    )?;

    merge_typed_field(
        name,
        existing,
        "env",
        config.env.as_ref().map(|env_map| {
            let mut m = serde_yaml_ng::Mapping::new();
            for (k, v) in env_map {
                m.insert(Value::String(k.clone()), Value::String(v.clone()));
            }
            Value::Mapping(m)
        }),
    )?;

    merge_typed_field(name, existing, "worktree", config.worktree.map(Value::Bool))?;

    Ok(())
}

fn merge_string_field(
    name: &str,
    existing: &mut serde_yaml_ng::Mapping,
    field: &str,
    daemon_value: &Option<String>,
) -> Result<(), FieldConflict> {
    use serde_yaml_ng::Value;

    let Some(new_v) = daemon_value else {
        return Ok(());
    };
    let class = instance_field_class(field);
    let key = Value::String(field.to_string());
    match (class, existing.get(&key)) {
        (FieldClass::DaemonManaged, _) | (FieldClass::OperatorHandEdit, None) => {
            existing.insert(key, Value::String(new_v.clone()));
            Ok(())
        }
        (FieldClass::OperatorHandEdit, Some(Value::String(old_v))) if old_v == new_v => Ok(()),
        (FieldClass::OperatorHandEdit, Some(old)) => {
            let old_str = match old {
                Value::String(s) => s.clone(),
                other => format!("{other:?}"),
            };
            Err(FieldConflict {
                instance: name.to_string(),
                field: field.to_string(),
                operator_value: old_str,
                daemon_value: new_v.clone(),
            })
        }
    }
}

fn merge_typed_field(
    name: &str,
    existing: &mut serde_yaml_ng::Mapping,
    field: &str,
    new_value: Option<serde_yaml_ng::Value>,
) -> Result<(), FieldConflict> {
    use serde_yaml_ng::Value;

    let Some(new_value) = new_value else {
        return Ok(());
    };
    let key = Value::String(field.into());
    let class = instance_field_class(field);
    match (class, existing.get(&key)) {
        (FieldClass::DaemonManaged, _) | (FieldClass::OperatorHandEdit, None) => {
            existing.insert(key, new_value);
            Ok(())
        }
        (FieldClass::OperatorHandEdit, Some(old)) if old == &new_value => Ok(()),
        (FieldClass::OperatorHandEdit, Some(old)) => Err(FieldConflict {
            instance: name.to_string(),
            field: field.to_string(),
            operator_value: format!("{old:?}"),
            daemon_value: format!("{new_value:?}"),
        }),
    }
}

pub(super) fn build_instance_mapping(config: &InstanceYamlEntry) -> serde_yaml_ng::Mapping {
    let mut inst = serde_yaml_ng::Mapping::new();
    for (key, val) in [
        ("backend", &config.backend),
        ("working_directory", &config.working_directory),
        ("role", &config.role),
        ("instructions", &config.instructions),
        ("source_repo", &config.source_repo),
        ("repo", &config.repo),
        ("github_login", &config.github_login),
        ("model", &config.model),
        ("model_tier", &config.model_tier),
        ("ready_pattern", &config.ready_pattern),
        ("command", &config.command),
        ("skills_path", &config.skills_path),
    ] {
        if let Some(ref v) = val {
            inst.insert(key.into(), serde_yaml_ng::Value::String(v.clone()));
        }
    }
    if let Some(ref args) = config.args {
        let seq: Vec<serde_yaml_ng::Value> = args
            .iter()
            .map(|s| serde_yaml_ng::Value::String(s.clone()))
            .collect();
        inst.insert("args".into(), serde_yaml_ng::Value::Sequence(seq));
    }
    if let Some(ref env_map) = config.env {
        let mut env_yaml = serde_yaml_ng::Mapping::new();
        for (k, v) in env_map {
            env_yaml.insert(
                serde_yaml_ng::Value::String(k.clone()),
                serde_yaml_ng::Value::String(v.clone()),
            );
        }
        inst.insert("env".into(), serde_yaml_ng::Value::Mapping(env_yaml));
    }
    if let Some(worktree) = config.worktree {
        inst.insert("worktree".into(), serde_yaml_ng::Value::Bool(worktree));
    }
    if let Some(ref mode) = config.topic_binding_mode {
        inst.insert(
            "topic_binding_mode".into(),
            serde_yaml_ng::Value::String(mode.clone()),
        );
    }
    inst
}
