//! Checks docs/API.md for the promises builders implement from the routes table: no route can
//! confirm a cloud charge or a terminate, and the table renders as one table.

const API: &str = include_str!("../../../docs/API.md");

/// Rows of the table under "## Routes", and whatever non-blank line ends it.
fn routes() -> (Vec<&'static str>, Option<&'static str>) {
    let section = API
        .split("## Routes")
        .nth(1)
        .expect("API.md has a Routes section");
    let mut rows = Vec::new();
    for line in section.lines().map(str::trim).skip_while(|l| l.is_empty()) {
        if line.starts_with('|') {
            rows.push(line);
        } else if !line.is_empty() {
            return (rows, Some(line));
        } else if !rows.is_empty() {
            break;
        }
    }
    (rows, None)
}

fn row(route: &str) -> &'static str {
    routes()
        .0
        .into_iter()
        .find(|r| r.contains(route))
        .unwrap_or_else(|| panic!("no routes row for {route}"))
}

#[test]
fn no_route_confirms_a_charge() {
    for r in routes().0 {
        let path = r.split('|').nth(1).unwrap_or_default();
        assert!(
            !path.contains("confirm"),
            "a route path confirms something: {r}"
        );
    }
    let start = row("`POST /api/v1/cloud/runpod/pods`");
    assert!(
        start.contains("the daemon itself raises"),
        "pod start row: {start}"
    );
    assert!(
        start.contains("No route accepts a confirmation"),
        "pod start row: {start}"
    );
    assert!(
        !start.contains("desktop app"),
        "pod start row hands the confirmation to the app: {start}"
    );
}

#[test]
fn terminate_needs_the_daemon_dialog() {
    let del = row("`DELETE /api/v1/cloud/runpod/pods/{id}`");
    assert!(
        del.contains("daemon raises the same native dialog"),
        "terminate row: {del}"
    );
    assert!(
        !del.contains(r#"`{"confirm":true}`"#),
        "terminate row accepts a body confirm: {del}"
    );
}

#[test]
fn routes_table_is_unbroken() {
    let (rows, _) = routes();
    assert!(
        rows.len() > 20,
        "routes table cut short after {} rows",
        rows.len()
    );
    assert!(
        rows.last().unwrap().contains("/v1/chat/completions"),
        "the OpenAI row must close the table"
    );
}
