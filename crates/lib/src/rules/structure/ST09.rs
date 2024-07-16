use ahash::AHashMap;
use itertools::Itertools;
use smol_str::{SmolStr, StrExt, ToSmolStr};

use crate::core::config::Value;
use crate::core::parser::segments::base::{ErasedSegment, SymbolSegment, SymbolSegmentNewArgs};
use crate::core::rules::base::{Erased, ErasedRule, LintFix, LintResult, Rule, RuleGroups};
use crate::core::rules::context::RuleContext;
use crate::core::rules::crawlers::{Crawler, SegmentSeekerCrawler};
use crate::dialects::ansi::{FromExpressionElementSegment, JoinClauseSegment};
use crate::utils::functional::context::FunctionalContext;
use crate::utils::functional::segments::Segments;

#[derive(Default, Debug, Clone)]
pub struct RuleST09 {
    preferred_first_table_in_join_clause: String,
}

impl Rule for RuleST09 {
    fn load_from_config(&self, config: &AHashMap<String, Value>) -> Result<ErasedRule, String> {
        Ok(RuleST09 {
            preferred_first_table_in_join_clause: config["preferred_first_table_in_join_clause"]
                .as_string()
                .unwrap()
                .to_owned(),
        }
        .erased())
    }

    fn name(&self) -> &'static str {
        "structure.join_condition_order"
    }

    fn description(&self) -> &'static str {
        "Joins should list the table referenced earlier/later first."
    }

    fn long_description(&self) -> &'static str {
        r#"
**Anti-pattern**

In this example, the tables that were referenced later are listed first
and the `preferred_first_table_in_join_clause` configuration
is set to `earlier`.

```sql
select
    foo.a,
    foo.b,
    bar.c
from foo
left join bar
    -- This subcondition does not list
    -- the table referenced earlier first:
    on bar.a = foo.a
    -- Neither does this subcondition:
    and bar.b = foo.b
```

**Best practice**

List the tables that were referenced earlier first.

```sql
select
    foo.a,
    foo.b,
    bar.c
from foo
left join bar
    on foo.a = bar.a
    and foo.b = bar.b
```
"#
    }

    fn groups(&self) -> &'static [RuleGroups] {
        &[RuleGroups::All, RuleGroups::Structure]
    }

    fn eval(&self, context: RuleContext) -> Vec<LintResult> {
        let mut table_aliases = Vec::new();
        let children = FunctionalContext::new(context.clone()).segment().children(None);
        let join_clauses = children.recursive_crawl(&["join_clause"], true);
        let join_on_conditions =
            join_clauses.children(None).recursive_crawl(&["join_on_condition"], true);

        if join_on_conditions.is_empty() {
            return Vec::new();
        }

        let from_expression_alias = FromExpressionElementSegment(
            children.recursive_crawl(&["from_expression_element"], true)[0].clone(),
        )
        .eventual_alias()
        .ref_str
        .clone();

        table_aliases.push(from_expression_alias);

        let mut join_clause_aliases = join_clauses
            .into_iter()
            .map(|join_clause| {
                JoinClauseSegment(join_clause).eventual_aliases().first().unwrap().1.ref_str.clone()
            })
            .collect_vec();

        table_aliases.append(&mut join_clause_aliases);

        let table_aliases = table_aliases.iter().map(|it| it.to_uppercase_smolstr()).collect_vec();
        let mut conditions = Vec::new();

        let join_on_condition_expressions =
            join_on_conditions.children(None).recursive_crawl(&["expression"], true);

        for expression in join_on_condition_expressions {
            let mut expression_group = Vec::new();
            for element in Segments::new(expression, None).children(None) {
                if !matches!(element.get_type(), "whitespace" | "newline") {
                    expression_group.push(element);
                }
            }
            conditions.push(expression_group);
        }

        let mut subconditions = Vec::new();

        for expression_group in conditions {
            subconditions.append(&mut split_list_by_segment_type(
                expression_group,
                "binary_operator".into(),
                vec!["and".into(), "or".into()],
            ));
        }

        let column_operator_column_subconditions = subconditions
            .into_iter()
            .filter(|it| is_qualified_column_operator_qualified_column_sequence(it))
            .collect_vec();

        let mut fixes = Vec::new();

        for subcondition in column_operator_column_subconditions {
            let comparison_operator = subcondition[1].clone();
            let first_column_reference = subcondition[0].clone();
            let second_column_reference = subcondition[2].clone();
            let raw_comparison_operators =
                comparison_operator.children(&["raw_comparison_operator"]);
            let first_table_seg =
                first_column_reference.child(&["naked_identifier", "quoted_identifier"]).unwrap();
            let second_table_seg =
                second_column_reference.child(&["naked_identifier", "quoted_identifier"]).unwrap();

            let first_table = first_table_seg.get_raw_upper().unwrap().to_smolstr();
            let second_table = second_table_seg.get_raw_upper().unwrap().to_smolstr();

            let raw_comparison_operator_opposites = |op| match op {
                "<" => ">",
                ">" => "<",
                _ => unimplemented!(),
            };

            if !table_aliases.contains(&first_table) || !table_aliases.contains(&second_table) {
                continue;
            }

            if (table_aliases.iter().position(|x| x == &first_table).unwrap()
                > table_aliases.iter().position(|x| x == &second_table).unwrap()
                && self.preferred_first_table_in_join_clause == "earlier")
                || (table_aliases.iter().position(|x| x == &first_table).unwrap()
                    < table_aliases.iter().position(|x| x == &second_table).unwrap()
                    && self.preferred_first_table_in_join_clause == "later")
            {
                fixes.push(LintFix::replace(
                    first_column_reference.clone(),
                    vec![second_column_reference.clone()],
                    None,
                ));
                fixes.push(LintFix::replace(
                    second_column_reference.clone(),
                    vec![first_column_reference.clone()],
                    None,
                ));

                if matches!(raw_comparison_operators[0].raw().as_ref(), "<" | ">")
                    && raw_comparison_operators.iter().map(|it| it.raw()).ne(["<", ">"])
                {
                    fixes.push(LintFix::replace(
                        raw_comparison_operators[0].clone(),
                        vec![SymbolSegment::create(
                            raw_comparison_operator_opposites(
                                raw_comparison_operators[0].raw().as_ref(),
                            ),
                            None,
                            SymbolSegmentNewArgs { r#type: "raw_comparison_operator" },
                        )],
                        None,
                    ));
                }
            }
        }

        if fixes.is_empty() {
            return Vec::new();
        }

        vec![LintResult::new(
            context.segment.into(),
            fixes,
            None,
            format!(
                "Joins should list the table referenced {}",
                self.preferred_first_table_in_join_clause
            )
            .into(),
            None,
        )]
    }

    fn crawl_behaviour(&self) -> Crawler {
        SegmentSeekerCrawler::new(["from_expression"].into()).into()
    }
}

fn split_list_by_segment_type(
    segment_list: Vec<ErasedSegment>,
    delimiter_type: SmolStr,
    delimiters: Vec<SmolStr>,
) -> Vec<Vec<ErasedSegment>> {
    let delimiters = delimiters.into_iter().map(|it| it.to_uppercase_smolstr()).collect_vec();
    let mut new_list = Vec::new();
    let mut sub_list = Vec::new();

    for i in 0..segment_list.len() {
        if i == segment_list.len() - 1 {
            sub_list.push(segment_list[i].clone());
            new_list.push(sub_list.clone());
        } else if segment_list[i].get_type() == delimiter_type
            && delimiters.contains(&segment_list[i].get_raw_upper().unwrap().to_smolstr())
        {
            new_list.push(sub_list.clone());
            sub_list.clear();
        } else {
            sub_list.push(segment_list[i].clone());
        }
    }

    new_list
}

fn is_qualified_column_operator_qualified_column_sequence(segment_list: &[ErasedSegment]) -> bool {
    if segment_list.len() != 3 {
        return false;
    }

    if segment_list[0].get_type() == "column_reference"
        && segment_list[0].direct_descendant_type_set().contains("dot")
        && segment_list[1].get_type() == "comparison_operator"
        && segment_list[2].get_type() == "column_reference"
        && segment_list[2].direct_descendant_type_set().contains("dot")
    {
        return true;
    }

    false
}
