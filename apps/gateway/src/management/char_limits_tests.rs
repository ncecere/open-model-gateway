//! #405: management text limits count characters (Unicode scalar values), as
//! the OpenAPI contract's `maxLength` does, never UTF-8 bytes. Each validator
//! accepts exactly the limit in two-, three- and four-byte characters and
//! refuses one character more.
use super::*;

const GLYPHS: [&str; 3] = ["\u{e9}", "\u{20ac}", "\u{1F600}"];

fn at(glyph: &str, n: usize) -> String {
    glyph.repeat(n)
}

#[test]
fn names_count_characters() {
    for g in GLYPHS {
        assert!(valid_name(&at(g, 120)), "{g:?} x120");
        assert!(!valid_name(&at(g, 121)), "{g:?} x121");
    }
    assert!(!valid_name(&" ".repeat(10)));
}

#[test]
fn descriptions_count_characters() {
    for g in GLYPHS {
        assert!(description_valid(&at(g, 2000)), "{g:?} x2000");
        assert!(!description_valid(&at(g, 2001)), "{g:?} x2001");
    }
}

#[test]
fn emails_count_characters() {
    for g in GLYPHS {
        let local = |n: usize| format!("{}@t.test", at(g, n - 7));
        assert!(directory::email_valid(&local(320)), "{g:?} 320");
        assert!(!directory::email_valid(&local(321)), "{g:?} 321");
    }
}

#[test]
fn group_mappings_count_characters() {
    let mapping = |issuer: String, group: String| directory::MappingInput {
        issuer,
        group_value: group,
        target_kind: "platform".into(),
        platform_role: Some("user".into()),
        workspace_id: None,
        workspace_role: None,
        enabled: true,
    };
    let prefix = "https://issuer.example.test/";
    for g in GLYPHS {
        let issuer = |n: usize| format!("{prefix}{}", at(g, n - prefix.len()));
        assert!(
            directory::mapping_valid(&mapping(issuer(2048), "g".into())),
            "{g:?} issuer 2048"
        );
        assert!(
            !directory::mapping_valid(&mapping(issuer(2049), "g".into())),
            "{g:?} issuer 2049"
        );
        let ok = format!("{prefix}x");
        assert!(
            directory::mapping_valid(&mapping(ok.clone(), at(g, 512))),
            "{g:?} group 512"
        );
        assert!(
            !directory::mapping_valid(&mapping(ok, at(g, 513))),
            "{g:?} group 513"
        );
    }
}

#[test]
fn provider_cost_text_counts_characters() {
    for g in GLYPHS {
        for limit in [200, 256, 512, 4096] {
            assert!(
                provider_costs::parser::bounded(&at(g, limit), limit),
                "{g:?} {limit}"
            );
            assert!(
                !provider_costs::parser::bounded(&at(g, limit + 1), limit),
                "{g:?} {limit}+1"
            );
        }
    }
}

#[test]
fn compare_ids_count_characters() {
    // Two ids padded with ideographic spaces (trimmed): 148 characters fit.
    let id = || uuid::Uuid::new_v4().to_string();
    let pad = "\u{3000}".repeat((148 - 73) / 2);
    let raw = format!("{pad}{},{}{pad}", id(), id());
    assert!(raw.chars().count() <= 148 && raw.len() > 148);
    assert!(compare::parse_ids(&raw).is_ok());
    let over = format!("{raw}\u{3000}\u{3000}");
    assert!(over.chars().count() > 148);
    assert!(compare::parse_ids(&over).is_err());
}
