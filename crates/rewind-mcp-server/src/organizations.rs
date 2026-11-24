//! The organization of a call: its `org` argument among the caller's organizations
//! (`archi/07-code-architecture.md`, "MCP and OAuth").
//!
//! `org` is an organization's id, or its name, compared without regard to ASCII case, when no
//! other of the caller's organizations has it. It may be left out when the caller reaches one
//! organization only. What cannot be resolved is said to the model with the choices it has.

use rewind_agent_tools::Organization;
use serde_json::Value;

/// One line per organization: its name and id.
fn choices<'a>(organizations: impl IntoIterator<Item = &'a Organization>) -> String {
    organizations
        .into_iter()
        .map(|organization| match &organization.name {
            Some(name) => format!("- {name} (id {})", organization.id),
            None => format!("- id {}", organization.id),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The organization `org` names, or what the model reads instead.
pub(crate) fn resolve<'a>(
    tool: &str,
    organizations: &'a [Organization],
    org: Option<&Value>,
) -> Result<&'a Organization, String> {
    let given = match org {
        None | Some(Value::Null) => None,
        Some(Value::String(org)) if !org.trim().is_empty() => Some(org.trim()),
        Some(_) => {
            return Err(format!(
                "{tool}: `org` must be a non-empty string: an organization's id or name, as \
                 list_orgs gives them."
            ));
        }
    };
    let Some(given) = given else {
        return match organizations {
            [] => Err(format!(
                "{tool}: you reach no organization: ask the person to open one to this \
                 connection in Rewind's dashboard."
            )),
            [one] => Ok(one),
            several => Err(format!(
                "{tool}: you reach {} organizations: give one as `org`, by its id or its name:\n{}",
                several.len(),
                choices(several)
            )),
        };
    };
    if let Some(found) = organizations
        .iter()
        .find(|organization| organization.id == given)
    {
        return Ok(found);
    }
    let named: Vec<&Organization> = organizations
        .iter()
        .filter(|organization| {
            organization
                .name
                .as_deref()
                .is_some_and(|name| name.trim().eq_ignore_ascii_case(given))
        })
        .collect();
    match named.as_slice() {
        [one] => Ok(one),
        [] => Err(format!(
            "{tool}: you reach no organization {given:?}. Give one of these as `org`, by its id \
             or its name:\n{}",
            choices(organizations)
        )),
        several => Err(format!(
            "{tool}: {} of the organizations you reach are named {given:?}: give one of their ids \
             as `org`:\n{}",
            several.len(),
            choices(several.iter().copied())
        )),
    }
}

#[cfg(test)]
mod tests {
    use rewind_api::access::Permission;
    use serde_json::json;

    use super::*;

    fn organization(id: &str, name: Option<&str>) -> Organization {
        Organization {
            id: id.into(),
            name: name.map(str::to_owned),
            permissions: vec![Permission::RunsRead],
        }
    }

    #[test]
    fn one_organization_needs_no_org() {
        let one = [organization("org_1", Some("Acme"))];
        assert_eq!(resolve("get_run", &one, None).unwrap().id, "org_1");
        assert_eq!(
            resolve("get_run", &one, Some(&Value::Null)).unwrap().id,
            "org_1"
        );
        assert_eq!(
            resolve("get_run", &[], None).unwrap_err(),
            "get_run: you reach no organization: ask the person to open one to this connection in Rewind's dashboard."
        );
    }

    #[test]
    fn several_organizations_are_named_by_id_or_by_name() {
        let several = [
            organization("org_1", Some("Acme")),
            organization("org_2", None),
            organization("org_3", Some("Globex")),
        ];
        assert_eq!(
            resolve("list_runs", &several, None).unwrap_err(),
            "list_runs: you reach 3 organizations: give one as `org`, by its id or its name:\n- Acme (id org_1)\n- id org_2\n- Globex (id org_3)"
        );
        assert_eq!(
            resolve("list_runs", &several, Some(&json!("org_2")))
                .unwrap()
                .id,
            "org_2"
        );
        assert_eq!(
            resolve("list_runs", &several, Some(&json!("globex")))
                .unwrap()
                .id,
            "org_3"
        );
        assert_eq!(
            resolve("list_runs", &several, Some(&json!("Initech"))).unwrap_err(),
            "list_runs: you reach no organization \"Initech\". Give one of these as `org`, by its id or its name:\n- Acme (id org_1)\n- id org_2\n- Globex (id org_3)"
        );
        assert_eq!(
            resolve("list_runs", &several, Some(&json!(3))).unwrap_err(),
            "list_runs: `org` must be a non-empty string: an organization's id or name, as list_orgs gives them."
        );
    }

    #[test]
    fn a_name_two_organizations_share_is_refused_with_both_ids() {
        let twins = [
            organization("org_1", Some("Acme")),
            organization("org_2", Some("ACME")),
        ];
        assert_eq!(
            resolve("run_workflow", &twins, Some(&json!("acme"))).unwrap_err(),
            "run_workflow: 2 of the organizations you reach are named \"acme\": give one of their ids as `org`:\n- Acme (id org_1)\n- ACME (id org_2)"
        );
        assert_eq!(
            resolve("run_workflow", &twins, Some(&json!("org_2")))
                .unwrap()
                .id,
            "org_2"
        );
    }
}
