//! Property tests of the Bedrock Converse request encoder `encode` (#593).
//!
//! The contract these tests state, read from `encode`:
//!
//! * It never panics. Its only errors are `InvalidRequest` (malformed or
//!   out-of-bounds input), `Unsupported` (a capability Bedrock's Converse
//!   mapping cannot carry: a strict tool, `tool_choice: none`, the developer
//!   role, error-marked tool results) and `InvalidField(name)` (a Messages
//!   option this provider class refuses: image, strict, top_k, output_config
//!   format, a bad or denied beta header). Each fault below is refused with its
//!   own class, not a catch-all.
//! * Whatever it accepts is carried whole: every system message becomes a
//!   system block, every other message becomes content in order (tool results
//!   of one turn merged into one user message, as documented), every tool
//!   call keeps its id, name and JSON arguments, every tool keeps its name,
//!   description and schema, the tool choice maps one to one, and the
//!   temperature, token limit, `top_p` and stop sequences are present exactly
//!   when set. Nothing is invented and nothing is dropped.
//! * Messages options other than `top_p` and `stop_sequences` (thinking, cache
//!   controls, context management, ...) are not forwarded; the code documents
//!   this as deliberate for translated providers, so it is not asserted either
//!   way here.
//! * It is deterministic.
//!
//! `ChatRequest` has no `Debug` (prompt-bearing), so the generators produce a
//! `Debug` spec that the test builds into a request.
use super::*;
use crate::proptest_support_tests::{arb_json, config, hostile};
use proptest::prelude::*;
use serde_json::json;

// ---- spec ------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Turn {
    User(String),
    Assistant {
        text: Option<String>,
        calls: Vec<Value>,
        order: Vec<u32>,
        results: Vec<String>,
    },
}

#[derive(Clone, Debug)]
enum Choice {
    Unset,
    Auto,
    Required,
    Function(usize),
}

#[derive(Clone, Debug)]
struct Spec {
    system: Vec<String>,
    first: String,
    turns: Vec<Turn>,
    tools: Vec<(String, Option<String>, Value)>,
    choice: Choice,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    options: Option<(Option<f64>, Option<Vec<String>>)>,
}

fn text(min: usize) -> impl Strategy<Value = String> {
    prop_oneof![
        3 => proptest::string::string_regex(&format!("[ -~\n]{{{min},40}}")).unwrap(),
        1 => prop::collection::vec(any::<char>(), min..=40)
            .prop_map(|v| v.into_iter().collect::<String>()),
    ]
}

fn args() -> impl Strategy<Value = Value> {
    arb_json().prop_map(|v| {
        if v.is_object() {
            v
        } else {
            json!({ "value": v })
        }
    })
}

fn turn() -> impl Strategy<Value = Turn> {
    prop_oneof![
        1 => text(1).prop_map(Turn::User),
        2 => (
            prop::option::of(text(0)),
            prop::collection::vec(args(), 0..4),
            prop::collection::vec(any::<u32>(), 4),
            prop::collection::vec(text(0), 4),
        )
            .prop_map(|(text, calls, order, results)| Turn::Assistant { text, calls, order, results }),
    ]
}

fn spec() -> impl Strategy<Value = Spec> {
    (
        prop::collection::vec(text(1), 0..3),
        text(1),
        prop::collection::vec(turn(), 0..6),
        prop::collection::vec(
            ("[A-Za-z0-9_-]{1,20}", prop::option::of(text(0)), args()),
            0..5,
        ),
        prop_oneof![
            Just(Choice::Unset),
            Just(Choice::Auto),
            Just(Choice::Required),
            any::<usize>().prop_map(Choice::Function),
        ],
        prop::option::of(0.0..=1.0_f64),
        prop::option::of(1..=i32::MAX as u32),
        prop::option::of((
            prop::option::of(0.0..=1.0_f64),
            prop::option::of(prop::collection::vec(text(0), 0..3)),
        )),
    )
        .prop_map(
            |(system, first, turns, tools, choice, temperature, max_tokens, options)| Spec {
                system,
                first,
                turns,
                tools,
                choice,
                temperature,
                max_tokens,
                options,
            },
        )
}

fn msg(role: Role, content: Option<String>) -> Message {
    Message {
        role,
        content,
        tool_calls: vec![],
        tool_call_id: None,
    }
}

fn build(spec: &Spec) -> ChatRequest {
    let mut messages: Vec<Message> = spec
        .system
        .iter()
        .map(|s| msg(Role::System, Some(s.clone())))
        .collect();
    messages.push(msg(Role::User, Some(spec.first.clone())));
    let mut next = 0;
    for turn in &spec.turns {
        match turn {
            Turn::User(t) => messages.push(msg(Role::User, Some(t.clone()))),
            Turn::Assistant {
                text,
                calls,
                order,
                results,
            } => {
                let mut text = text.clone();
                if calls.is_empty() && text.as_deref().is_none_or(str::is_empty) {
                    text = Some("a".into());
                }
                let tool_calls: Vec<ToolCall> = calls
                    .iter()
                    .map(|args| {
                        next += 1;
                        ToolCall {
                            id: format!("call_{next}"),
                            name: format!("fn-{}", next % 3),
                            arguments: args.to_string(),
                        }
                    })
                    .collect();
                let mut m = msg(Role::Assistant, text);
                m.tool_calls = tool_calls.clone();
                messages.push(m);
                let mut idx: Vec<usize> = (0..tool_calls.len()).collect();
                idx.sort_by_key(|i| order[i % order.len()]);
                for i in idx {
                    let mut r = msg(Role::Tool, Some(results[i % results.len()].clone()));
                    r.tool_call_id = Some(tool_calls[i].id.clone());
                    messages.push(r);
                }
            }
        }
    }
    if messages.last().is_some_and(|m| m.role == Role::Assistant) {
        messages.push(msg(Role::User, Some("tail".into())));
    }
    let tools: Vec<FunctionTool> = spec
        .tools
        .iter()
        .enumerate()
        .map(|(i, (name, description, props))| FunctionTool {
            name: format!("{name}_{i}"),
            description: description.clone(),
            parameters: json!({"type": "object", "properties": props}),
            strict: if i % 2 == 0 { None } else { Some(false) },
        })
        .collect();
    let tool_choice = if tools.is_empty() {
        None
    } else {
        match spec.choice {
            Choice::Unset => None,
            Choice::Auto => Some(ToolChoice::Auto),
            Choice::Required => Some(ToolChoice::Required),
            Choice::Function(i) => Some(ToolChoice::Function(tools[i % tools.len()].name.clone())),
        }
    };
    ChatRequest {
        messages_options: spec.options.as_ref().map(|(top_p, stop)| {
            Box::new(MessagesOptions {
                top_p: *top_p,
                stop_sequences: stop.clone(),
                ..MessagesOptions::default()
            })
        }),
        model: "public-model".into(),
        messages,
        tools,
        tool_choice,
        temperature: spec.temperature,
        max_output_tokens: spec.max_tokens,
        stream: false,
    }
}

// ---- oracle ----------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Block {
    Text(String),
    Use {
        id: String,
        name: String,
        input: Value,
    },
    Result {
        id: String,
        text: String,
    },
}

/// `(is_assistant, blocks)` per Converse message, from the request alone, by
/// the documented rule: text and tool calls in order, tool results of one turn
/// merged into the user message that already holds results.
fn expected_messages(request: &ChatRequest) -> Vec<(bool, Vec<Block>)> {
    let mut out: Vec<(bool, Vec<Block>)> = vec![];
    for m in request.messages.iter().filter(|m| m.role != Role::System) {
        let blocks = if m.role == Role::Tool {
            vec![Block::Result {
                id: m.tool_call_id.clone().unwrap(),
                text: m.content.clone().unwrap(),
            }]
        } else {
            let mut b = vec![];
            if let Some(t) = m.content.as_ref().filter(|t| !t.is_empty()) {
                b.push(Block::Text(t.clone()));
            }
            for c in &m.tool_calls {
                b.push(Block::Use {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    input: serde_json::from_str(&c.arguments).unwrap(),
                });
            }
            b
        };
        let merge = m.role == Role::Tool
            && out
                .last()
                .is_some_and(|(a, b)| !a && b.iter().all(|b| matches!(b, Block::Result { .. })));
        if merge {
            out.last_mut().unwrap().1.extend(blocks);
        } else {
            out.push((m.role == Role::Assistant, blocks));
        }
    }
    out
}

fn value_of(d: &Document) -> Value {
    match d {
        Document::Null => Value::Null,
        Document::Bool(b) => Value::Bool(*b),
        Document::String(s) => Value::String(s.clone()),
        Document::Array(a) => Value::Array(a.iter().map(value_of).collect()),
        Document::Object(o) => {
            Value::Object(o.iter().map(|(k, v)| (k.clone(), value_of(v))).collect())
        }
        Document::Number(Number::PosInt(n)) => Value::from(*n),
        Document::Number(Number::NegInt(n)) => Value::from(*n),
        Document::Number(Number::Float(f)) => {
            serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number)
        }
    }
}

fn actual_messages(input: &Input) -> Vec<(bool, Vec<Block>)> {
    input
        .messages
        .iter()
        .map(|m| {
            let blocks = m
                .content
                .iter()
                .map(|b| match b {
                    aws::ContentBlock::Text(t) => Block::Text(t.clone()),
                    aws::ContentBlock::ToolUse(u) => Block::Use {
                        id: u.tool_use_id.clone(),
                        name: u.name.clone(),
                        input: value_of(&u.input),
                    },
                    aws::ContentBlock::ToolResult(r) => {
                        let [aws::ToolResultContentBlock::Text(text)] = r.content.as_slice() else {
                            panic!("a tool result with other content")
                        };
                        Block::Result {
                            id: r.tool_use_id.clone(),
                            text: text.clone(),
                        }
                    }
                    _ => panic!("a content block the request never had"),
                })
                .collect();
            (m.role == aws::ConversationRole::Assistant, blocks)
        })
        .collect()
}

/// Everything an accepted request carries is in the encoded input.
fn check_carried(request: &ChatRequest, input: &Input) -> std::result::Result<(), TestCaseError> {
    // System blocks.
    let system: Vec<String> = request
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.clone().unwrap())
        .collect();
    let got: Option<Vec<String>> = input.system.as_ref().map(|s| {
        s.iter()
            .map(|b| match b {
                aws::SystemContentBlock::Text(t) => t.clone(),
                _ => panic!("a system block the request never had"),
            })
            .collect()
    });
    prop_assert_eq!(got, (!system.is_empty()).then_some(system));
    // Conversation.
    prop_assert_eq!(actual_messages(input), expected_messages(request));
    // Tools and choice.
    match &input.tools {
        None => {
            prop_assert!(request.tools.is_empty());
            prop_assert!(request.tool_choice.is_none());
        }
        Some(config) => {
            prop_assert_eq!(config.tools.len(), request.tools.len());
            for (got, want) in config.tools.iter().zip(&request.tools) {
                let aws::Tool::ToolSpec(spec) = got else {
                    panic!("a tool kind the request never had")
                };
                prop_assert_eq!(&spec.name, &want.name);
                prop_assert_eq!(&spec.description, &want.description);
                let Some(aws::ToolInputSchema::Json(schema)) = &spec.input_schema else {
                    panic!("no schema")
                };
                prop_assert_eq!(value_of(schema), want.parameters.clone());
            }
            match (&config.tool_choice, &request.tool_choice) {
                (None, None) => {}
                (Some(aws::ToolChoice::Auto(_)), Some(ToolChoice::Auto)) => {}
                (Some(aws::ToolChoice::Any(_)), Some(ToolChoice::Required)) => {}
                (Some(aws::ToolChoice::Tool(t)), Some(ToolChoice::Function(n))) => {
                    prop_assert_eq!(&t.name, n)
                }
                _ => prop_assert!(false, "tool choice not carried one to one"),
            }
        }
    }
    // Sampling and limits are present exactly when set.
    let options = request.messages_options.as_deref();
    prop_assert_eq!(
        input.inference.temperature,
        request.temperature.map(|t| t as f32)
    );
    prop_assert_eq!(
        input.inference.max_tokens,
        request.max_output_tokens.map(|n| n as i32)
    );
    prop_assert_eq!(
        input.inference.top_p,
        options.and_then(|o| o.top_p).map(|p| p as f32)
    );
    prop_assert_eq!(
        &input.inference.stop_sequences,
        &options.and_then(|o| o.stop_sequences.clone())
    );
    Ok(())
}

fn same(a: &Result<Input>, b: &Result<Input>) -> bool {
    match (a, b) {
        (Ok(a), Ok(b)) => {
            a.messages == b.messages
                && a.system == b.system
                && a.inference == b.inference
                && a.tools == b.tools
        }
        (Err(a), Err(b)) => a == b,
        _ => false,
    }
}

fn refusal_class(e: &InferenceError) -> bool {
    matches!(
        e,
        InferenceError::InvalidRequest
            | InferenceError::Unsupported
            | InferenceError::InvalidField(_)
    )
}

// ---- faults ----------------------------------------------------------------

#[derive(Clone, Debug)]
enum Fault {
    NoMessages,
    TooManyMessages,
    TooManyTools,
    Temperature(f64),
    MaxTokens(u32),
    StrictTool,
    BadToolName(String),
    DuplicateToolName,
    ParametersNotAnObject(Value),
    SchemaTypeNotObject(Value),
    ChoiceNoneWithTools,
    ChoiceWithoutTools(u8),
    ChoiceUnknownFunction,
    DeveloperRole(usize),
    LateSystem,
    EmptySystem(bool),
    OrphanToolResult,
    DuplicateToolResult,
    MissingToolResult,
    ToolCallOnUser,
    ToolCallIdOnUser,
    UserWithoutContent(bool),
    FirstIsAssistant,
    LastIsAssistant,
    Oversize,
    DuplicateCallId,
    BadCallId(String),
    BadCallArguments(String),
    OptionStrict,
    OptionImages,
    OptionTopK,
    OptionErrorResults,
    OptionOutputFormat,
    OptionBadBeta(String),
}

fn fault() -> impl Strategy<Value = Fault> {
    let bad_name = prop::sample::select(vec!["", "has space", "dot.name", "ünï", "a/b"])
        .prop_map(str::to_owned);
    let long = Just("x".repeat(65));
    let nonobject = hostile().prop_filter("not an object", |v| !v.is_object());
    let bad_args = prop::sample::select(vec![
        "", "{", "[1]", "\"s\"", "null", "1", "{\"a\":", "{} {}",
    ])
    .prop_map(str::to_owned);
    let bad_beta = prop::sample::select(vec![
        "context-1m-2025",
        "bad\u{1}header",
        "",
        "a,,b",
        "web-search-2025",
    ])
    .prop_map(str::to_owned);
    let simple = prop::sample::select(vec![
        Fault::NoMessages,
        Fault::TooManyMessages,
        Fault::TooManyTools,
        Fault::StrictTool,
        Fault::DuplicateToolName,
        Fault::ChoiceNoneWithTools,
        Fault::ChoiceUnknownFunction,
        Fault::LateSystem,
        Fault::OrphanToolResult,
        Fault::DuplicateToolResult,
        Fault::MissingToolResult,
        Fault::ToolCallOnUser,
        Fault::ToolCallIdOnUser,
        Fault::FirstIsAssistant,
        Fault::LastIsAssistant,
        Fault::Oversize,
        Fault::DuplicateCallId,
        Fault::OptionStrict,
        Fault::OptionImages,
        Fault::OptionTopK,
        Fault::OptionErrorResults,
        Fault::OptionOutputFormat,
    ]);
    let limits = prop_oneof![
        prop::sample::select(vec![
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -0.1,
            1.0000001,
            2.0,
            -1e308
        ])
        .prop_map(Fault::Temperature),
        prop::sample::select(vec![0_u32, i32::MAX as u32 + 1, u32::MAX]).prop_map(Fault::MaxTokens),
        prop_oneof![bad_name, long].prop_map(Fault::BadToolName),
        nonobject.prop_map(Fault::ParametersNotAnObject),
        prop_oneof![
            Just(json!("array")),
            Just(json!(1)),
            Just(Value::Null),
            Just(json!(["object"]))
        ]
        .prop_map(Fault::SchemaTypeNotObject),
        (0..4_u8).prop_map(Fault::ChoiceWithoutTools),
    ];
    let messages = prop_oneof![
        any::<usize>().prop_map(Fault::DeveloperRole),
        any::<bool>().prop_map(Fault::EmptySystem),
        any::<bool>().prop_map(Fault::UserWithoutContent),
        prop::sample::select(vec!["", "has space", "a.b", "é"])
            .prop_map(|s| Fault::BadCallId(s.into())),
        bad_args.prop_map(Fault::BadCallArguments),
        bad_beta.prop_map(Fault::OptionBadBeta),
    ];
    prop_oneof![3 => simple, 2 => limits, 2 => messages]
}

fn a_tool(name: &str) -> FunctionTool {
    FunctionTool {
        name: name.into(),
        description: None,
        parameters: json!({"type": "object", "properties": {}}),
        strict: None,
    }
}

fn a_call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "fn-0".into(),
        arguments: "{}".into(),
    }
}

fn options(r: &mut ChatRequest) -> &mut MessagesOptions {
    r.messages_options.get_or_insert_with(Default::default)
}

/// Break exactly one rule of a valid request, returning the refusal it must get.
fn inject(r: &mut ChatRequest, fault: &Fault) -> InferenceError {
    use InferenceError::{InvalidField, InvalidRequest, Unsupported};
    let user = |t: &str| msg(Role::User, Some(t.into()));
    match fault {
        Fault::NoMessages => {
            r.messages.clear();
            InvalidRequest
        }
        Fault::TooManyMessages => {
            r.messages = (0..1025).map(|_| user("x")).collect();
            InvalidRequest
        }
        Fault::TooManyTools => {
            r.tools = (0..129).map(|i| a_tool(&format!("t{i}"))).collect();
            r.tool_choice = None;
            InvalidRequest
        }
        Fault::Temperature(t) => {
            r.temperature = Some(*t);
            InvalidRequest
        }
        Fault::MaxTokens(n) => {
            r.max_output_tokens = Some(*n);
            InvalidRequest
        }
        Fault::StrictTool => {
            if r.tools.is_empty() {
                r.tools.push(a_tool("t0"));
            }
            r.tools[0].strict = Some(true);
            Unsupported
        }
        Fault::BadToolName(n) => {
            r.tools.push(a_tool(n));
            InvalidRequest
        }
        Fault::DuplicateToolName => {
            if r.tools.is_empty() {
                r.tools.push(a_tool("t0"));
            }
            let again = r.tools[0].clone();
            r.tools.push(again);
            InvalidRequest
        }
        Fault::ParametersNotAnObject(v) => {
            r.tools.push(a_tool("t_bad"));
            r.tools.last_mut().unwrap().parameters = v.clone();
            InvalidRequest
        }
        Fault::SchemaTypeNotObject(t) => {
            r.tools.push(a_tool("t_bad"));
            r.tools.last_mut().unwrap().parameters = json!({ "type": t });
            InvalidRequest
        }
        Fault::ChoiceNoneWithTools => {
            if r.tools.is_empty() {
                r.tools.push(a_tool("t0"));
            }
            r.tool_choice = Some(ToolChoice::None);
            Unsupported
        }
        Fault::ChoiceWithoutTools(k) => {
            r.tools.clear();
            r.tool_choice = Some(match k {
                0 => ToolChoice::Auto,
                1 => ToolChoice::Required,
                2 => ToolChoice::None,
                _ => ToolChoice::Function("t0".into()),
            });
            InvalidRequest
        }
        Fault::ChoiceUnknownFunction => {
            if r.tools.is_empty() {
                r.tools.push(a_tool("t0"));
            }
            r.tool_choice = Some(ToolChoice::Function("zz_not_a_tool".into()));
            InvalidRequest
        }
        Fault::DeveloperRole(at) => {
            let at = at % (r.messages.len() + 1);
            r.messages
                .insert(at, msg(Role::Developer, Some("d".into())));
            Unsupported
        }
        Fault::LateSystem => {
            r.messages.push(msg(Role::System, Some("late".into())));
            InvalidRequest
        }
        Fault::EmptySystem(none) => {
            r.messages.insert(
                0,
                msg(Role::System, if *none { None } else { Some(String::new()) }),
            );
            InvalidRequest
        }
        Fault::OrphanToolResult => {
            let mut m = msg(Role::Tool, Some("r".into()));
            m.tool_call_id = Some("never_called".into());
            r.messages.push(m);
            InvalidRequest
        }
        Fault::DuplicateToolResult => {
            let mut a = msg(Role::Assistant, None);
            a.tool_calls = vec![a_call("dup_result")];
            let mut t = msg(Role::Tool, Some("r".into()));
            t.tool_call_id = Some("dup_result".into());
            r.messages.extend([a, t.clone(), t]);
            InvalidRequest
        }
        Fault::MissingToolResult => {
            let mut a = msg(Role::Assistant, None);
            a.tool_calls = vec![a_call("no_result")];
            r.messages.extend([a, user("after")]);
            InvalidRequest
        }
        Fault::ToolCallOnUser => {
            let mut u = user("u");
            u.tool_calls = vec![a_call("on_user")];
            r.messages.push(u);
            InvalidRequest
        }
        Fault::ToolCallIdOnUser => {
            let mut u = user("u");
            u.tool_call_id = Some("x".into());
            r.messages.push(u);
            InvalidRequest
        }
        Fault::UserWithoutContent(none) => {
            r.messages.push(msg(
                Role::User,
                if *none { None } else { Some(String::new()) },
            ));
            InvalidRequest
        }
        Fault::FirstIsAssistant => {
            let at = r
                .messages
                .iter()
                .position(|m| m.role != Role::System)
                .unwrap_or(0);
            r.messages
                .insert(at, msg(Role::Assistant, Some("a".into())));
            InvalidRequest
        }
        Fault::LastIsAssistant => {
            r.messages.push(msg(Role::Assistant, Some("a".into())));
            InvalidRequest
        }
        Fault::Oversize => {
            r.messages.push(user(&"x".repeat(OUTPUT_LIMIT + 1)));
            InvalidRequest
        }
        Fault::DuplicateCallId => {
            let mut a = msg(Role::Assistant, None);
            a.tool_calls = vec![a_call("dup_call")];
            let mut t = msg(Role::Tool, Some("r".into()));
            t.tool_call_id = Some("dup_call".into());
            r.messages.extend([a.clone(), t.clone(), a, t]);
            InvalidRequest
        }
        Fault::BadCallId(id) => {
            let mut a = msg(Role::Assistant, None);
            a.tool_calls = vec![a_call(id)];
            r.messages.push(a);
            InvalidRequest
        }
        Fault::BadCallArguments(args) => {
            let mut a = msg(Role::Assistant, None);
            let mut call = a_call("bad_args");
            call.arguments = args.clone();
            a.tool_calls = vec![call];
            r.messages.push(a);
            InvalidRequest
        }
        Fault::OptionStrict => {
            options(r).strict_tools = true;
            InvalidField("strict")
        }
        Fault::OptionImages => {
            options(r).images = vec![json!({"type": "image"})];
            InvalidField("image")
        }
        Fault::OptionTopK => {
            options(r).top_k = Some(5);
            InvalidField("top_k")
        }
        Fault::OptionErrorResults => {
            options(r).error_results = true;
            Unsupported
        }
        Fault::OptionOutputFormat => {
            options(r).output_config = Some(json!({"format": {"type": "json_schema"}}));
            InvalidField("output_config.format")
        }
        Fault::OptionBadBeta(b) => {
            options(r).beta_headers = vec![b.clone()];
            InvalidField("anthropic-beta")
        }
    }
}

// ---- mutations of a valid request -------------------------------------------

#[derive(Clone, Debug)]
enum Edit {
    Drop(usize),
    Duplicate(usize),
    Swap(usize, usize),
    Truncate(usize),
    Role(usize, u8),
    Content(usize, u8, String),
    ToolCallId(usize, Option<String>),
    ClearCalls(usize),
    CallArgs(usize, String),
    CallId(usize, String),
    DropTool(usize),
    ToolParams(usize, Value),
    ToolStrict(usize, Option<bool>),
    Choice(u8, usize),
    Temperature(Option<f64>),
    MaxTokens(Option<u32>),
}

fn edit() -> impl Strategy<Value = Edit> {
    let messages = prop_oneof![
        any::<usize>().prop_map(Edit::Drop),
        any::<usize>().prop_map(Edit::Duplicate),
        (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Edit::Swap(a, b)),
        any::<usize>().prop_map(Edit::Truncate),
        (any::<usize>(), 0..5_u8).prop_map(|(i, r)| Edit::Role(i, r)),
        (any::<usize>(), 0..3_u8, text(0)).prop_map(|(i, k, t)| Edit::Content(i, k, t)),
        (any::<usize>(), prop::option::of("[a-z_]{0,4}"))
            .prop_map(|(i, id)| Edit::ToolCallId(i, id)),
        any::<usize>().prop_map(Edit::ClearCalls),
        (
            any::<usize>(),
            prop::sample::select(vec!["{}", "[]", "", "{\"a\":1}", "nope"]).prop_map(str::to_owned)
        )
            .prop_map(|(i, a)| Edit::CallArgs(i, a)),
        (any::<usize>(), "[a-z_ ]{0,4}").prop_map(|(i, id)| Edit::CallId(i, id)),
    ];
    let options = prop_oneof![
        any::<usize>().prop_map(Edit::DropTool),
        (any::<usize>(), hostile()).prop_map(|(i, v)| Edit::ToolParams(i, v)),
        (any::<usize>(), prop::option::of(any::<bool>())).prop_map(|(i, s)| Edit::ToolStrict(i, s)),
        (0..5_u8, any::<usize>()).prop_map(|(k, i)| Edit::Choice(k, i)),
        prop::option::of(prop::sample::select(vec![
            0.0,
            0.7,
            1.0,
            1.5,
            f64::NAN,
            -1.0
        ]))
        .prop_map(Edit::Temperature),
        prop::option::of(prop::sample::select(vec![
            0_u32,
            1,
            4096,
            i32::MAX as u32,
            u32::MAX
        ]))
        .prop_map(Edit::MaxTokens),
    ];
    prop_oneof![3 => messages, 1 => options]
}

fn edit_request(r: &mut ChatRequest, e: &Edit) {
    let n = r.messages.len().max(1);
    let t = r.tools.len().max(1);
    match e {
        Edit::Drop(i) if !r.messages.is_empty() => {
            r.messages.remove(i % n);
        }
        Edit::Duplicate(i) if !r.messages.is_empty() => {
            let m = r.messages[i % n].clone();
            r.messages.insert(i % n, m);
        }
        Edit::Swap(a, b) if !r.messages.is_empty() => r.messages.swap(a % n, b % n),
        Edit::Truncate(len) => r.messages.truncate(len % (r.messages.len() + 1)),
        Edit::Role(i, role) if !r.messages.is_empty() => {
            r.messages[i % n].role = [
                Role::System,
                Role::Developer,
                Role::User,
                Role::Assistant,
                Role::Tool,
            ][*role as usize];
        }
        Edit::Content(i, kind, text) if !r.messages.is_empty() => {
            r.messages[i % n].content = match kind {
                0 => None,
                1 => Some(String::new()),
                _ => Some(text.clone()),
            };
        }
        Edit::ToolCallId(i, id) if !r.messages.is_empty() => {
            r.messages[i % n].tool_call_id = id.clone()
        }
        Edit::ClearCalls(i) if !r.messages.is_empty() => r.messages[i % n].tool_calls.clear(),
        Edit::CallArgs(i, a) if !r.messages.is_empty() => {
            if let Some(c) = r.messages[i % n].tool_calls.first_mut() {
                c.arguments = a.clone();
            }
        }
        Edit::CallId(i, id) if !r.messages.is_empty() => {
            if let Some(c) = r.messages[i % n].tool_calls.first_mut() {
                c.id = id.clone();
            }
        }
        Edit::DropTool(i) if !r.tools.is_empty() => {
            r.tools.remove(i % t);
        }
        Edit::ToolParams(i, v) if !r.tools.is_empty() => r.tools[i % t].parameters = v.clone(),
        Edit::ToolStrict(i, s) if !r.tools.is_empty() => r.tools[i % t].strict = *s,
        Edit::Choice(k, i) => {
            r.tool_choice = match k {
                0 => None,
                1 => Some(ToolChoice::Auto),
                2 => Some(ToolChoice::Required),
                3 => Some(ToolChoice::None),
                _ if !r.tools.is_empty() => Some(ToolChoice::Function(r.tools[i % t].name.clone())),
                _ => Some(ToolChoice::Function("missing".into())),
            };
        }
        Edit::Temperature(t) => r.temperature = *t,
        Edit::MaxTokens(m) => r.max_output_tokens = *m,
        _ => {}
    }
}

// ---- properties ------------------------------------------------------------

proptest! {
    #![proptest_config(config(1200))]

    /// A well-formed request (bounded messages, tools, options) is accepted, is
    /// carried whole, and encodes the same twice.
    #[test]
    fn a_valid_request_is_accepted_carried_whole_and_deterministic(spec in spec()) {
        let request = build(&spec);
        let first = encode(&request);
        let second = encode(&request);
        prop_assert!(same(&first, &second), "not deterministic");
        match first {
            Ok(input) => check_carried(&request, &input)?,
            Err(e) => prop_assert!(false, "a valid request was refused: {e:?}"),
        }
    }

    /// For any request however mangled (messages dropped, duplicated,
    /// swapped, truncated; roles, content, ids and arguments changed; tools,
    /// choice and limits edited) `encode` does not panic, refuses only with
    /// `InvalidRequest`, `Unsupported` or `InvalidField`, is deterministic, and
    /// whenever it accepts, the encoded input carries the request whole.
    #[test]
    fn a_mutated_request_is_refused_explicitly_or_carried_whole(
        spec in spec(),
        edits in prop::collection::vec(edit(), 1..4),
    ) {
        let mut request = build(&spec);
        for e in &edits {
            edit_request(&mut request, e);
        }
        let first = encode(&request);
        prop_assert!(same(&first, &encode(&request)), "not deterministic");
        match first {
            Ok(input) => check_carried(&request, &input)?,
            Err(e) => prop_assert!(refusal_class(&e), "unexpected refusal {e:?}"),
        }
    }

    /// Breaking exactly one rule of a valid request is refused with that
    /// rule's own class: `Unsupported` for what Bedrock cannot carry,
    /// `InvalidField` naming the Messages option, `InvalidRequest` for the rest.
    #[test]
    fn each_fault_is_refused_with_its_own_class(spec in spec(), fault in fault()) {
        let mut request = build(&spec);
        prop_assert!(encode(&request).is_ok());
        let expected = inject(&mut request, &fault);
        match encode(&request) {
            Err(got) => prop_assert_eq!(got, expected, "{:?}", fault),
            Ok(_) => prop_assert!(false, "{:?} was accepted", fault),
        }
    }
}

/// The documented limits refuse rather than truncate: exactly `MAX_TOOLS`
/// tools, exactly 1024 messages and exactly the output limit are accepted and
/// carried; one more of each is refused.
#[test]
fn the_limits_are_inclusive_and_never_truncate() {
    let mut r = build(&spec_min());
    r.tools = (0..MAX_TOOLS).map(|i| a_tool(&format!("t{i}"))).collect();
    let input = encode(&r).expect("exactly MAX_TOOLS tools");
    check_carried(&r, &input).unwrap();
    assert_eq!(input.tools.unwrap().tools.len(), MAX_TOOLS);
    r.tools.push(a_tool("one_too_many"));
    assert_eq!(encode(&r).err(), Some(InferenceError::InvalidRequest));

    let mut r = build(&spec_min());
    r.messages[0].content = Some("x".repeat(OUTPUT_LIMIT));
    let input = encode(&r).expect("exactly the output limit");
    check_carried(&r, &input).unwrap();
    r.messages[0].content = Some("x".repeat(OUTPUT_LIMIT + 1));
    assert_eq!(encode(&r).err(), Some(InferenceError::InvalidRequest));

    let mut r = build(&spec_min());
    r.messages = (0..1024)
        .map(|_| msg(Role::User, Some("m".into())))
        .collect();
    let input = encode(&r).expect("exactly 1024 messages");
    check_carried(&r, &input).unwrap();
    assert_eq!(input.messages.len(), 1024);
    r.messages.push(msg(Role::User, Some("m".into())));
    assert_eq!(encode(&r).err(), Some(InferenceError::InvalidRequest));
}

fn spec_min() -> Spec {
    Spec {
        system: vec![],
        first: "hello".into(),
        turns: vec![],
        tools: vec![],
        choice: Choice::Unset,
        temperature: None,
        max_tokens: None,
        options: None,
    }
}
