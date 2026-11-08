//! `demo.hello`: a fake operation that greets and tabulates.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::guard::Class;
use crate::ops::{Choice, Input, Meta, OpResult, Operation, Param, ParamType, Runner, View};

/// Prompts for a name and renders a summary plus a table, with two artificial
/// steps so progress reporting has something to show.
#[derive(Clone, Copy, Debug, Default)]
pub struct Hello;

const STEP_DELAY: Duration = Duration::from_millis(150);

#[async_trait]
impl Operation for Hello {
    fn meta(&self) -> Meta {
        Meta {
            id: "demo.hello",
            name: "Hello (demo)",
            summary: "Fake operation exercising the full pipeline end to end",
            category: "Demo",
            class: Class::Read,
        }
    }

    fn params(&self) -> Vec<Param> {
        vec![
            Param::new("name")
                .label("Name")
                .required(true)
                .help("Who to greet")
                .suggest(Arc::new(|_deps, _input| {
                    Box::pin(async {
                        Ok(vec![
                            choice("world", "world — the classic"),
                            choice("support", "support — the audience"),
                            choice("danny", "danny — the owner"),
                        ])
                    })
                })),
            Param::new("repeat")
                .label("Repeat")
                .ty(ParamType::Int)
                .default("3")
                .help("Rows to generate"),
            Param::new("shout")
                .label("Shout")
                .ty(ParamType::Bool)
                .default("false")
                .help("Uppercase the greeting"),
        ]
    }

    async fn run(&self, r: &mut Runner, input: &Input) -> anyhow::Result<OpResult> {
        let repeat = match input.int("repeat") {
            n if n <= 0 => 1,
            n => n,
        };
        let mut greeting = format!("Hello, {}!", input.str("name"));
        if input.bool("shout") {
            greeting = greeting.to_uppercase();
        }

        r.step("compose greeting", |_| async {
            tokio::time::sleep(STEP_DELAY).await;
            Ok(())
        })
        .await?;

        let rows = r
            .step("tabulate", |_| async {
                tokio::time::sleep(STEP_DELAY).await;
                Ok((1..=repeat)
                    .map(|i| vec![i.to_string(), greeting.clone()])
                    .collect::<Vec<_>>())
            })
            .await?;

        let env = match r.deps.env.name.as_str() {
            "" => "(none)",
            name => name,
        };
        Ok(OpResult {
            views: vec![
                View::summary([
                    ("Greeting", greeting.as_str()),
                    ("Rows", repeat.to_string().as_str()),
                    ("Environment", env),
                ]),
                View::Table {
                    title: "Greetings".into(),
                    cols: vec!["#".into(), "Message".into()],
                    rows,
                },
            ],
        })
    }
}

fn choice(value: &str, label: &str) -> Choice {
    Choice {
        value: value.into(),
        label: Some(label.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{Deps, Input, Runner, View};

    async fn run(input: Input) -> Vec<View> {
        Runner::new(Deps::default())
            .execute(&Hello, input)
            .await
            .expect("hello succeeds")
            .views
    }

    #[tokio::test]
    async fn hello_greets_and_tabulates() {
        let views = run([("name", "world")].into_iter().collect()).await;
        assert_eq!(
            views,
            vec![
                View::summary([
                    ("Greeting", "Hello, world!"),
                    ("Rows", "3"),
                    ("Environment", "(none)"),
                ]),
                View::Table {
                    title: "Greetings".into(),
                    cols: vec!["#".into(), "Message".into()],
                    rows: (1..=3)
                        .map(|i| vec![i.to_string(), "Hello, world!".into()])
                        .collect(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn hello_lenient_repeat_and_shout() {
        let views = run([("name", "x"), ("repeat", "abc"), ("shout", "true")]
            .into_iter()
            .collect())
        .await;
        assert_eq!(
            views[0],
            View::summary([
                ("Greeting", "HELLO, X!"),
                ("Rows", "1"),
                ("Environment", "(none)"),
            ])
        );
        let View::Table { rows, .. } = &views[1] else {
            panic!("second view is a table");
        };
        assert_eq!(rows, &vec![vec!["1".to_string(), "HELLO, X!".to_string()]]);
    }

    #[tokio::test]
    async fn hello_suggests_three_names() {
        let params = Hello.params();
        let suggest = params[0].suggest.clone().expect("name has suggestions");
        let choices = suggest(Deps::default(), Input::default())
            .await
            .expect("suggestions");
        let values: Vec<_> = choices.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, ["world", "support", "danny"]);
        assert_eq!(choices[0].label.as_deref(), Some("world — the classic"));
    }
}
