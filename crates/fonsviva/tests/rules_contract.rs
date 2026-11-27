//! Checks AGENTS.md and docs/ARCHITECTURE.md keep the extension reach rules builders implement:
//! reach out of the org and reach into it both stay off until an admin allows them, and extension
//! code never runs with the org server's privileges.

const AGENTS: &str = include_str!("../../../AGENTS.md");
const ARCHITECTURE: &str = include_str!("../../../docs/ARCHITECTURE.md");

/// The doc with its line breaks folded, so a rule wrapped across lines still matches.
fn flat(doc: &str) -> String {
    doc.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn reach_out_of_the_org_needs_an_admin() {
    for (name, doc) in [("AGENTS.md", AGENTS), ("ARCHITECTURE.md", ARCHITECTURE)] {
        let doc = flat(doc);
        assert!(
            doc.contains("off until an admin allows that destination"),
            "{name} lost the rule that an extension's outbound reach is off until an admin allows it"
        );
    }
}

#[test]
fn reach_into_the_org_needs_an_admin() {
    for (name, doc) in [("AGENTS.md", AGENTS), ("ARCHITECTURE.md", ARCHITECTURE)] {
        let doc = flat(doc);
        assert!(
            doc.contains("stays off until an admin allows it"),
            "{name} lost the rule that an inbound webhook or MCP endpoint is off until an admin allows it"
        );
    }
}

#[test]
fn extension_code_never_gets_server_privileges() {
    for (name, doc) in [("AGENTS.md", AGENTS), ("ARCHITECTURE.md", ARCHITECTURE)] {
        let doc = flat(doc);
        assert!(
            doc.contains("never runs with the org server's privileges"),
            "{name} lost the rule that extension code never runs with the org server's privileges"
        );
        assert!(
            doc.contains("only the capabilities its `fonsviva.json` declares"),
            "{name} lost the rule that extension code gets only its declared capabilities"
        );
    }
}

#[test]
fn installs_without_reach_need_no_approval() {
    let doc = flat(AGENTS);
    assert!(
        !doc.contains("no store and no approval step"),
        "AGENTS.md says installs need no approval at all, which drops the admin's allow for reach"
    );
}
