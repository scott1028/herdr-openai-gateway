//! Relay a backing agent's TUI dialog to the client: parse the blocked screen,
//! map the user's answer to an option, and build the relay payloads. Pure, no IO.

use serde_json::{json, Value};

use crate::openai::ToolDef;

const CURSOR_MARKERS: [char; 2] = ['\u{276f}', '\u{203a}']; // ❯ ›
const RULE_CHARS: [char; 2] = ['\u{2500}', '-']; // ─ -
const BORDER_CHARS: [char; 6] = [
    '\u{2502}', '\u{256d}', '\u{256e}', '\u{2570}', '\u{256f}', '\u{2500}',
]; // │╭╮╰╯─
   // Dashed separators (e.g. Claude's file-preview rules): skipped, unlike a
   // solid ─ rule which ends the question walk.
const DASHED_RULE_CHARS: [char; 7] = [
    '\u{254c}', '\u{254d}', '\u{2504}', '\u{2505}', '\u{2508}', '\u{2509}', '\u{2550}',
]; // ╌ ╍ ┄ ┅ ┈ ┉ ═
const MAX_QUESTION_LINES: usize = 12;
const FALLBACK_QUESTION_LINES: usize = 15;
pub(crate) const HEADER_MAX_CHARS: usize = 30;

/// A dialog parsed from a blocked agent screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog {
    pub question: String,
    pub options: Vec<String>,
    pub cursor: usize,
}

/// Parse herdr `agent.read --source detection --format text` output (the bottom
/// buffer, plain text). Without a cursor line, or with fewer than two options,
/// the dialog carries no options and the question falls back to the screen tail.
pub fn parse_dialog(screen: &str) -> Dialog {
    let lines: Vec<&str> = screen.lines().map(|line| line.trim_end()).collect();
    let Some(index) = lines.iter().rposition(|line| is_cursor_line(line)) else {
        return fallback_dialog(&lines);
    };
    let column = cursor_text_column(lines[index]);

    let mut first = index;
    while first > 0 && option_line(lines[first - 1], column) {
        first -= 1;
    }
    let mut last = index;
    while last + 1 < lines.len() && option_line(lines[last + 1], column) {
        last += 1;
    }
    let options: Vec<String> = lines[first..=last]
        .iter()
        .map(|line| strip_number_prefix(text_at_column(line, column)).to_string())
        .collect();
    if options.len() < 2 {
        return fallback_dialog(&lines);
    }
    Dialog {
        question: collect_question(&lines, first),
        options,
        cursor: index - first,
    }
}

/// A cursor line starts (after trimming) with `❯` or `›`, a space, then text.
fn is_cursor_line(line: &str) -> bool {
    line.trim_start()
        .strip_prefix(CURSOR_MARKERS)
        .is_some_and(|rest| rest.starts_with(' ') && rest.len() > 1)
}

/// Char column where a cursor line's text starts, after the marker and spaces.
fn cursor_text_column(line: &str) -> usize {
    let spaces = line
        .trim_start()
        .chars()
        .skip(1)
        .take_while(|c| *c == ' ')
        .count();
    text_indent_width(line) + 1 + spaces
}

/// A sibling option line is non-blank with its text starting at the option column.
fn option_line(line: &str, column: usize) -> bool {
    !line.is_empty() && text_indent_width(line) == column
}

/// The substring of `line` starting at char column `column` (empty if shorter).
fn text_at_column(line: &str, column: usize) -> &str {
    match line.char_indices().nth(column) {
        Some((byte, _)) => &line[byte..],
        None => "",
    }
}

fn text_indent_width(line: &str) -> usize {
    line.chars().count() - line.trim_start().chars().count()
}

/// Walk up from above the options block; blank lines are skipped, a rule line
/// ends the walk, and at most MAX_QUESTION_LINES lines are collected.
fn collect_question(lines: &[&str], options_first: usize) -> String {
    let mut collected: Vec<&str> = Vec::new();
    let mut index = options_first;
    while index > 0 {
        index -= 1;
        let line = lines[index];
        if line.is_empty() {
            continue;
        }
        if is_rule_line(line) || collected.len() >= MAX_QUESTION_LINES {
            break;
        }
        if is_border_line(line) || is_dashed_line(line) {
            continue;
        }
        collected.push(line.trim());
    }
    collected.reverse();
    collected.join("\n")
}

fn fallback_dialog(lines: &[&str]) -> Dialog {
    let mut tail: Vec<&str> = lines
        .iter()
        .rev()
        .filter(|line| !line.is_empty())
        .take(FALLBACK_QUESTION_LINES)
        .map(|line| line.trim())
        .collect();
    tail.reverse();
    Dialog {
        question: tail.join("\n"),
        options: Vec::new(),
        cursor: 0,
    }
}

fn is_rule_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| RULE_CHARS.contains(&c))
}

fn is_border_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| BORDER_CHARS.contains(&c))
}

fn is_dashed_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| DASHED_RULE_CHARS.contains(&c))
}

/// Strip a leading `1.` / `1)` number prefix and the spaces after it.
fn strip_number_prefix(label: &str) -> &str {
    let digits = label.trim_start_matches(|c: char| c.is_ascii_digit());
    match digits.strip_prefix(['.', ')']) {
        Some(rest) if digits.len() < label.len() && rest.starts_with([' ', '\t']) => {
            rest.trim_start_matches([' ', '\t'])
        }
        _ => label,
    }
}

/// Map a typed answer to an option index: `1..=N` by number, else an exact
/// label (case-insensitive), else the only label containing the text.
pub fn answer_from_text(text: &str, dialog: &Dialog) -> Option<usize> {
    let text = text.trim();
    if let Ok(number) = text.parse::<usize>() {
        return (number >= 1 && number <= dialog.options.len()).then(|| number - 1);
    }
    let lowered = text.to_lowercase();
    if let Some(index) = dialog
        .options
        .iter()
        .position(|option| option.to_lowercase() == lowered)
    {
        return Some(index);
    }
    if text.chars().count() < 2 {
        return None;
    }
    let matches: Vec<usize> = dialog
        .options
        .iter()
        .enumerate()
        .filter(|(_, option)| option.to_lowercase().contains(&lowered))
        .map(|(index, _)| index)
        .collect();
    if matches.len() == 1 {
        Some(matches[0])
    } else {
        None
    }
}

/// Map the opencode `question` tool result to an option index. The answer is
/// the value of the last `"="` pair, matched as an exact label; failing that,
/// the longest option label appearing in the content. Unanswered and dismissed
/// results are None.
pub fn answer_from_tool_result(content: &str, dialog: &Dialog) -> Option<usize> {
    if content.contains("Unanswered") || content.contains("The user dismissed this question") {
        return None;
    }
    if let Some(answer) = last_answer_value(content) {
        let lowered = answer.trim().to_lowercase();
        if let Some(index) = dialog
            .options
            .iter()
            .position(|option| option.to_lowercase() == lowered)
        {
            return Some(index);
        }
    }
    let content = content.to_lowercase();
    let mut best: Option<(usize, usize)> = None;
    for (index, option) in dialog.options.iter().enumerate() {
        let option = option.to_lowercase();
        if content.contains(&option) && best.is_none_or(|(_, length)| option.len() > length) {
            best = Some((index, option.len()));
        }
    }
    best.map(|(index, _)| index)
}

/// Value of the last `="value"` pair in the tool result content.
fn last_answer_value(content: &str) -> Option<&str> {
    let (_, after) = content.rmatch_indices("=\"").next()?;
    let end = after.find('"')?;
    Some(&after[..end])
}

/// The single herdr logical key (src/input/parse.rs) that moves the cursor one
/// step from the CONFIRMED cursor toward `to`. The service sends one key at a
/// time and re-derives the direction from the confirmed cursor after every
/// screen update, because menus can wrap and renders can lag.
pub fn next_move_key(from: usize, to: usize) -> Option<&'static str> {
    if to > from {
        Some("down")
    } else if to < from {
        Some("up")
    } else {
        None
    }
}

/// True when the client offers opencode's built-in `question` tool.
pub fn question_tool_offered(tools: &[ToolDef]) -> bool {
    tools.iter().any(|tool| {
        tool.function.name == "question"
            && tool
                .function
                .parameters
                .as_ref()
                .and_then(|parameters| parameters.get("properties"))
                .and_then(|properties| properties.get("questions"))
                .is_some()
    })
}

/// The `question` tool arguments in exactly opencode's shape.
pub fn relay_tool_arguments(agent_label: &str, dialog: &Dialog) -> Value {
    let question = if dialog.question.is_empty() {
        format!("{agent_label} is asking")
    } else {
        dialog.question.clone()
    };
    let header: String = format!("{agent_label} asks")
        .chars()
        .take(HEADER_MAX_CHARS)
        .collect();
    json!({
        "questions": [{
            "question": question,
            "header": header,
            "options": dialog
                .options
                .iter()
                .map(|option| json!({"label": option, "description": ""}))
                .collect::<Vec<_>>(),
            "multiple": false,
        }]
    })
}

/// Plain-text relay for clients without the `question` tool.
pub fn relay_text(
    agent_label: &str,
    tab_label: &str,
    dialog: &Dialog,
    note: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(note) = note {
        out.push_str(note);
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "{agent_label} (herdr tab {tab_label}) is asking:\n\n{}\n",
        dialog.question
    ));
    if dialog.options.is_empty() {
        out.push_str(&format!(
            "\nThis dialog needs input in the herdr TUI: open herdr tab {tab_label} and answer \
             it there; this conversation continues once the agent is free."
        ));
        return out;
    }
    for (index, option) in dialog.options.iter().enumerate() {
        out.push_str(&format!("\n{}. {option}", index + 1));
        if index == dialog.cursor {
            out.push_str("   (current)");
        }
    }
    out.push_str("\n\nReply with the number of your choice.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::FunctionDef;

    // Real Claude Code folder-trust screen, verbatim from
    // `herdr agent read --source detection --format text`.
    const TRUST_SCREEN: &str = concat!(
        "claude --tools '' --strict-mcp-config --session-id c736ab63-0a4d-4545-aeb9-8bdf10c628ab\n",
        "scott@scott-265K:~$ claude --tools '' --strict-mcp-config --session-id c736ab63-0a4d-4545-aeb9-8bdf10c628ab\n",
        "\n",
        "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n",
        " Accessing workspace:\n",
        "\n",
        " /home/scott\n",
        "\n",
        " Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team). If not, take a moment to\n",
        " review what's in this folder first.\n",
        "\n",
        " Claude Code'll be able to read, edit, and execute files here.\n",
        "\n",
        " Security guide\n",
        "\n",
        " \u{276f} No, exit\n",
        "   Yes, I trust this folder\n",
        "\n",
        " Enter to confirm \u{b7} Esc to cancel",
    );

    const PERMISSION_SCREEN: &str = concat!(
        "Do you want to create notes.txt?\n",
        "\n",
        " \u{276f} 1. Yes\n",
        "   2. Yes, allow all edits during this session (shift+tab)\n",
        "   3. No, and tell Claude what to do differently (esc)",
    );

    // Synthetic Codex-style menu: `›` marker, box borders around the question,
    // cursor on the second option.
    const CODEX_SCREEN: &str = concat!(
        "\u{256d}\u{2500} Continue with this command? \u{2500}\u{256e}\n",
        "\u{2502} git push origin main\n",
        "\u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}\n",
        "\n",
        "   Yes, proceed\n",
        " \u{203a} Yes, and don't ask again this session\n",
        "   No, and tell Codex what to do differently",
    );

    const LOGIN_SCREEN: &str = concat!(
        "Sign in to continue\n",
        "\n",
        "Open https://example.dev/device and paste this code:\n",
        "\n",
        "ABC-123",
    );

    fn trust_dialog() -> Dialog {
        parse_dialog(TRUST_SCREEN)
    }

    #[test]
    fn trust_screen_options_and_cursor() {
        let dialog = trust_dialog();
        assert_eq!(
            dialog.options,
            [
                "No, exit".to_string(),
                "Yes, I trust this folder".to_string()
            ]
        );
        assert_eq!(dialog.cursor, 0);
    }

    #[test]
    fn trust_screen_question_skips_rule_and_footer() {
        let dialog = trust_dialog();
        assert!(dialog.question.contains("Accessing workspace:"));
        assert!(dialog.question.contains("Quick safety check"));
        assert!(!dialog.question.contains("claude --tools"));
        assert!(!dialog.question.contains("Enter to confirm"));
    }

    #[test]
    fn numbered_permission_options_lose_their_prefixes() {
        let dialog = parse_dialog(PERMISSION_SCREEN);
        assert_eq!(
            dialog.options,
            [
                "Yes".to_string(),
                "Yes, allow all edits during this session (shift+tab)".to_string(),
                "No, and tell Claude what to do differently (esc)".to_string(),
            ]
        );
        assert_eq!(dialog.cursor, 0);
        assert_eq!(dialog.question, "Do you want to create notes.txt?");
    }

    #[test]
    fn dashed_separators_around_a_file_preview_are_skipped() {
        // Live Claude Code permission screen: `╌` rules frame the file preview;
        // they are separators, not part of the question.
        let dashes = "\u{254c}".repeat(40);
        let screen = format!(
            concat!(
                " Create file\n",
                " relay2.txt\n",
                "{dashes}\n",
                "  1 hello\n",
                "{dashes}\n",
                " Do you want to create relay2.txt?\n",
                " \u{276f} 1. Yes\n",
                "   2. Yes, and switch to accept edits (auto-approve file edits and \
                 common file commands) for this session (shift+tab)\n",
                "   3. No",
            ),
            dashes = dashes,
        );
        let dialog = parse_dialog(&screen);
        assert_eq!(
            dialog.question,
            "Create file\nrelay2.txt\n1 hello\nDo you want to create relay2.txt?"
        );
        assert_eq!(
            dialog.options,
            [
                "Yes".to_string(),
                "Yes, and switch to accept edits (auto-approve file edits and common \
                 file commands) for this session (shift+tab)"
                    .to_string(),
                "No".to_string(),
            ]
        );
        assert_eq!(dialog.cursor, 0);
    }

    #[test]
    fn codex_menu_with_second_option_selected_skips_border_lines() {
        let dialog = parse_dialog(CODEX_SCREEN);
        assert_eq!(
            dialog.options,
            [
                "Yes, proceed".to_string(),
                "Yes, and don't ask again this session".to_string(),
                "No, and tell Codex what to do differently".to_string(),
            ]
        );
        assert_eq!(dialog.cursor, 1);
        assert!(dialog.question.contains("Continue with this command?"));
        assert!(!dialog.question.contains('\u{2570}'));
    }

    #[test]
    fn screen_without_options_falls_back_to_the_screen_tail() {
        let dialog = parse_dialog(LOGIN_SCREEN);
        assert!(dialog.options.is_empty());
        assert_eq!(dialog.cursor, 0);
        assert_eq!(
            dialog.question,
            "Sign in to continue\nOpen https://example.dev/device and paste this code:\nABC-123"
        );
    }

    #[test]
    fn a_single_option_block_is_not_a_dialog() {
        let dialog = parse_dialog("Continue?\n\n \u{276f} Yes");
        assert!(dialog.options.is_empty());
        assert_eq!(dialog.cursor, 0);
        assert_eq!(dialog.question, "Continue?\n\u{276f} Yes");
    }

    #[test]
    fn question_stops_at_twelve_collected_lines() {
        let mut screen = String::new();
        for n in 1..=20 {
            screen.push_str(&format!("q{n:02}\n"));
        }
        screen.push_str("\n \u{276f} Yes\n   No");
        let dialog = parse_dialog(&screen);
        let lines: Vec<&str> = dialog.question.split('\n').collect();
        assert_eq!(lines.len(), 12);
        assert_eq!(lines[0], "q09");
        assert_eq!(lines[11], "q20");
    }

    #[test]
    fn numbered_answers_map_to_option_indexes() {
        let dialog = trust_dialog();
        assert_eq!(answer_from_text("2", &dialog), Some(1));
        assert_eq!(answer_from_text(" 2 ", &dialog), Some(1));
    }

    #[test]
    fn out_of_range_numbers_are_none() {
        let dialog = trust_dialog();
        assert_eq!(answer_from_text("0", &dialog), None);
        assert_eq!(answer_from_text("9", &dialog), None);
    }

    #[test]
    fn exact_label_matches_ignoring_case() {
        let dialog = trust_dialog();
        assert_eq!(
            answer_from_text("yes, i TRUST this folder", &dialog),
            Some(1)
        );
    }

    #[test]
    fn unique_substring_matches() {
        let dialog = trust_dialog();
        assert_eq!(answer_from_text("trust", &dialog), Some(1));
    }

    #[test]
    fn ambiguous_or_short_substrings_are_none() {
        let dialog = parse_dialog(PERMISSION_SCREEN);
        assert_eq!(answer_from_text("es", &dialog), None);
        assert_eq!(answer_from_text("x", &trust_dialog()), None);
    }

    #[test]
    fn tool_result_with_the_label_maps_to_the_option() {
        let dialog = trust_dialog();
        let content = "User has answered your questions: \"Accessing workspace: /tmp/x\"=\"Yes, I trust this folder\". You can now continue with the user's answers in mind.";
        assert_eq!(answer_from_tool_result(content, &dialog), Some(1));
        let lowered = "User has answered your questions: \"Accessing workspace: /tmp/x\"=\"no, exit\". You can now continue with the user's answers in mind.";
        assert_eq!(answer_from_tool_result(lowered, &dialog), Some(0));
    }

    #[test]
    fn unanswered_and_dismissed_tool_results_are_none() {
        let dialog = trust_dialog();
        let unanswered = "User has answered your questions: \"Accessing workspace: /tmp/x\"=\"Unanswered\". You can now continue with the user's answers in mind.";
        assert_eq!(answer_from_tool_result(unanswered, &dialog), None);
        assert_eq!(
            answer_from_tool_result("The user dismissed this question", &dialog),
            None
        );
    }

    #[test]
    fn unknown_custom_tool_result_answer_is_none() {
        let dialog = trust_dialog();
        let content = "User has answered your questions: \"Accessing workspace: /tmp/x\"=\"Only inside /home/scott/src\". You can now continue with the user's answers in mind.";
        assert_eq!(answer_from_tool_result(content, &dialog), None);
    }

    #[test]
    fn tool_result_falls_back_to_the_longest_label_in_the_content() {
        let dialog = trust_dialog();
        let content = "User has answered your questions: \"Which option?\"=\"the second one\". Options: No, exit | Yes, I trust this folder.";
        assert_eq!(answer_from_tool_result(content, &dialog), Some(1));
        let no_pair = "The user picked Yes, I trust this folder.";
        assert_eq!(answer_from_tool_result(no_pair, &dialog), Some(1));
    }

    #[test]
    fn next_move_key_picks_the_direction_from_the_confirmed_cursor() {
        assert_eq!(next_move_key(0, 3), Some("down"));
        assert_eq!(next_move_key(3, 1), Some("up"));
        assert_eq!(next_move_key(2, 2), None);
    }

    fn question_tool(parameters: Option<Value>) -> ToolDef {
        ToolDef {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "question".to_string(),
                description: None,
                parameters,
            },
        }
    }

    #[test]
    fn question_tool_is_recognised_by_name_and_questions_property() {
        let with_questions = question_tool(Some(json!({
            "type": "object",
            "properties": {"questions": {"type": "array"}},
        })));
        assert!(question_tool_offered(&[with_questions]));

        let without_questions = question_tool(Some(json!({"type": "object"})));
        let ask_user = ToolDef {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "ask_user".to_string(),
                description: None,
                parameters: Some(json!({"properties": {"questions": {}}})),
            },
        };
        assert!(!question_tool_offered(&[without_questions, ask_user]));
        assert!(!question_tool_offered(&[]));
    }

    #[test]
    fn relay_tool_arguments_match_opencode_shape() {
        let dialog = trust_dialog();
        let arguments = relay_tool_arguments("Claude Code", &dialog);
        let question = &arguments["questions"][0];
        assert_eq!(arguments["questions"].as_array().map(Vec::len), Some(1));
        assert!(question["question"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Accessing workspace:"));
        assert_eq!(question["header"], "Claude Code asks");
        assert_eq!(question["multiple"], json!(false));
        let options = question["options"].as_array().cloned().unwrap_or_default();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["label"], "No, exit");
        assert_eq!(options[0]["description"], "");
        assert_eq!(options[1]["label"], "Yes, I trust this folder");
    }

    #[test]
    fn relay_tool_arguments_fill_an_empty_question_and_truncate_the_header() {
        let dialog = Dialog {
            question: String::new(),
            options: vec!["Yes".to_string()],
            cursor: 0,
        };
        let arguments = relay_tool_arguments("Claude Code", &dialog);
        assert_eq!(
            arguments["questions"][0]["question"],
            "Claude Code is asking"
        );

        let long_label = "a".repeat(40);
        let arguments = relay_tool_arguments(&long_label, &dialog);
        let header = arguments["questions"][0]["header"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert_eq!(header, "a".repeat(30));
    }

    #[test]
    fn relay_text_matches_the_exact_layout() {
        let dialog = trust_dialog();
        let text = relay_text("Claude Code", "3", &dialog, None);
        assert_eq!(
            text,
            concat!(
                "Claude Code (herdr tab 3) is asking:\n",
                "\n",
                "Accessing workspace:\n",
                "/home/scott\n",
                "Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team). If not, take a moment to\n",
                "review what's in this folder first.\n",
                "Claude Code'll be able to read, edit, and execute files here.\n",
                "Security guide\n",
                "\n",
                "1. No, exit   (current)\n",
                "2. Yes, I trust this folder\n",
                "\n",
                "Reply with the number of your choice.",
            )
        );
    }

    #[test]
    fn relay_text_note_goes_first_and_the_cursor_moves_the_marker() {
        let dialog = trust_dialog();
        let text = relay_text("Claude Code", "3", &dialog, Some("The agent is blocked."));
        assert!(text.starts_with("The agent is blocked.\n\nClaude Code (herdr tab 3) is asking:"));

        let mut moved = dialog.clone();
        moved.cursor = 1;
        let text = relay_text("Claude Code", "3", &moved, None);
        assert!(text.contains("1. No, exit\n"));
        assert!(text.contains("2. Yes, I trust this folder   (current)"));
    }

    #[test]
    fn relay_text_without_options_points_at_the_herdr_tab() {
        let dialog = parse_dialog(LOGIN_SCREEN);
        let text = relay_text("Codex", "5", &dialog, None);
        assert_eq!(
            text,
            concat!(
                "Codex (herdr tab 5) is asking:\n",
                "\n",
                "Sign in to continue\n",
                "Open https://example.dev/device and paste this code:\n",
                "ABC-123\n",
                "\n",
                "This dialog needs input in the herdr TUI: open herdr tab 5 and answer it there; \
                 this conversation continues once the agent is free.",
            )
        );
    }
}
