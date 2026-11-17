//! `shipping`: a carrier's shipping labels, with typed results.
//!
//! Each `create_label` buys a new label and bills its postage: a label bought twice for an
//! order is paid twice. An order `P-…` is collected in store: its label has no `tracking`.

use std::sync::Mutex;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{Args, CallContext, Reply, Service, ToolSpec, lock, object_schema};
use crate::clock::rfc3339;

/// The postage of a label, by service, in cents.
pub const GROUND_CENTS: u64 = 650;
pub const EXPRESS_CENTS: u64 = 1_450;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tracking {
    pub carrier: String,
    pub number: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Label {
    pub label_id: String,
    pub order_id: String,
    pub service: String,
    pub postage_cents: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracking: Option<Tracking>,
    pub created_at: String,
}

/// Whether an order is collected in store: its label has no tracking number.
pub fn is_pickup(order_id: &str) -> bool {
    order_id.starts_with("P-")
}

/// The tracking number of the `n`th label (from 1) of an order: `1Z` and 12 digits.
pub fn tracking_number(order_id: &str, n: usize) -> String {
    let digits: String = order_id.chars().filter(char::is_ascii_digit).collect();
    format!("1Z{:0>8}{n:04}", digits)
}

#[derive(Debug, Default)]
pub struct Shipping {
    labels: Mutex<Vec<Label>>,
}

impl Shipping {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The labels of a snapshot of the server.
pub fn labels_in(state: &Value) -> Vec<Label> {
    state["labels"]
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .map(|label| Label {
                    label_id: label["label_id"].as_str().unwrap_or_default().to_owned(),
                    order_id: label["order_id"].as_str().unwrap_or_default().to_owned(),
                    service: label["service"].as_str().unwrap_or_default().to_owned(),
                    postage_cents: label["postage_cents"].as_u64().unwrap_or_default(),
                    tracking: label["tracking"]["number"].as_str().map(|number| Tracking {
                        carrier: label["tracking"]["carrier"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                        number: number.to_owned(),
                    }),
                    created_at: label["created_at"].as_str().unwrap_or_default().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn label_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "label_id": { "type": "string" },
            "order_id": { "type": "string" },
            "service": { "type": "string", "enum": ["ground", "express"] },
            "postage_cents": { "type": "integer", "description": "What the label cost." },
            "tracking": {
                "type": "object",
                "description": "Absent for an order collected in store.",
                "properties": { "carrier": { "type": "string" }, "number": { "type": "string" } },
                "required": ["carrier", "number"]
            },
            "created_at": { "type": "string", "format": "date-time" }
        },
        "required": ["label_id", "order_id", "service", "postage_cents", "created_at"]
    })
}

impl Service for Shipping {
    fn name(&self) -> &'static str {
        "shipping"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec::new(
                "create_label",
                "Buy a shipping label for an order, and bill its postage. Every call buys a new label. An order collected in store gets a label without tracking.",
                object_schema(
                    json!({
                        "order_id": { "type": "string" },
                        "service": { "type": "string", "enum": ["ground", "express"], "description": "ground by default" }
                    }),
                    &["order_id"],
                ),
            )
            .output(label_schema()),
            ToolSpec::new(
                "list_labels",
                "The labels bought, oldest first, those of one order when `order_id` is given.",
                object_schema(json!({ "order_id": { "type": "string" } }), &[]),
            )
            .output(json!({
                "type": "object",
                "properties": { "labels": { "type": "array", "items": label_schema() } },
                "required": ["labels"]
            })),
        ]
    }

    fn call(&self, context: &CallContext, tool: &str, args: &Map<String, Value>) -> Reply {
        let args = Args(args);
        let mut labels = lock(&self.labels);
        match tool {
            "create_label" => {
                let order_id = match args.str("order_id") {
                    Ok(order_id) => order_id.to_owned(),
                    Err(reply) => return reply,
                };
                let service = args.opt_str("service").unwrap_or("ground").to_owned();
                let postage_cents = if service == "express" {
                    EXPRESS_CENTS
                } else {
                    GROUND_CENTS
                };
                let of_order = labels
                    .iter()
                    .filter(|label| label.order_id == order_id)
                    .count();
                let tracking = (!is_pickup(&order_id)).then(|| Tracking {
                    carrier: "UPS".to_owned(),
                    number: tracking_number(&order_id, of_order + 1),
                });
                let label = Label {
                    label_id: format!("L-{}", 7001 + labels.len()),
                    order_id,
                    service,
                    postage_cents,
                    tracking,
                    created_at: rfc3339(context.now),
                };
                labels.push(label.clone());
                Reply::Structured(json!(label))
            }
            "list_labels" => {
                let order_id = args.opt_str("order_id");
                let listed: Vec<&Label> = labels
                    .iter()
                    .filter(|label| order_id.is_none_or(|order_id| label.order_id == order_id))
                    .collect();
                Reply::Structured(json!({ "labels": listed }))
            }
            other => Reply::error(format!("unknown tool {other}")),
        }
    }

    fn snapshot(&self) -> Value {
        json!({ "labels": *lock(&self.labels) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::T0;

    fn call(shipping: &Shipping, tool: &str, args: Value) -> Reply {
        let context = CallContext {
            now: T0,
            idempotency_key: None,
        };
        shipping.call(&context, tool, args.as_object().unwrap())
    }

    #[test]
    fn every_label_is_bought_and_a_pickup_has_no_tracking() {
        let shipping = Shipping::new();
        let Reply::Structured(first) =
            call(&shipping, "create_label", json!({ "order_id": "A-1003" }))
        else {
            panic!("a label");
        };
        assert_eq!(first["tracking"]["number"], "1Z000010030001");
        let Reply::Structured(pickup) = call(
            &shipping,
            "create_label",
            json!({ "order_id": "P-1004", "service": "express" }),
        ) else {
            panic!("a label");
        };
        assert!(pickup.get("tracking").is_none());
        assert_eq!(pickup["postage_cents"], EXPRESS_CENTS);
        call(&shipping, "create_label", json!({ "order_id": "A-1003" }));
        let labels = labels_in(&shipping.snapshot());
        assert_eq!(labels.len(), 3);
        assert_eq!(
            labels[2].tracking.as_ref().unwrap().number,
            tracking_number("A-1003", 2)
        );
        let Reply::Structured(listed) =
            call(&shipping, "list_labels", json!({ "order_id": "A-1003" }))
        else {
            panic!("labels");
        };
        assert_eq!(listed["labels"].as_array().unwrap().len(), 2);
    }
}
