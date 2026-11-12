//! `crm`: customers, their orders and refunds. Results are JSON in text.
//!
//! Customers `C-1001` to `C-1040`; `C-1004`, `C-1009` and `C-1013` have no company. Orders
//! `O-501` to `O-640`, except `O-504`, `O-509` and `O-511`, which do not exist.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{Args, CallContext, Fixed, Reply, Service, ToolSpec, lock, object_schema, page};
use crate::clock::rfc3339;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Company {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Customer {
    pub id: String,
    pub name: String,
    pub email: String,
    pub company: Option<Company>,
    /// `free`, `pro` or `enterprise`.
    pub tier: String,
    /// `amer`, `emea` or `apac`.
    pub region: String,
    pub lifetime_value_cents: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Order {
    pub id: String,
    pub customer_id: String,
    pub total_cents: u64,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Refund {
    pub refund_id: String,
    pub order_id: String,
    pub amount_cents: u64,
    pub reason: String,
    pub at: String,
}

const FIRST: [&str; 10] = [
    "Ada", "Bruno", "Chen", "Dalia", "Emil", "Fatou", "Goran", "Hiro", "Ines", "Jonas",
];
const LAST: [&str; 8] = [
    "Moreau",
    "Silva",
    "Okafor",
    "Lindqvist",
    "Tanaka",
    "Haddad",
    "Novak",
    "Reyes",
];
const COMPANIES: [&str; 10] = [
    "Northwind",
    "Globex",
    "Initech",
    "Umbrella",
    "Hooli",
    "Vandelay",
    "Stark",
    "Wayne",
    "Tyrell",
    "Soylent",
];

/// Customers without a company.
pub const WITHOUT_COMPANY: [&str; 3] = ["C-1004", "C-1009", "C-1013"];
/// Orders that do not exist.
pub const MISSING_ORDERS: [&str; 3] = ["O-504", "O-509", "O-511"];

pub fn customers() -> Vec<Customer> {
    let mut fixed = Fixed::new(23);
    (1001..=1040u64)
        .map(|n| {
            let id = format!("C-{n}");
            let first = fixed.pick(&FIRST);
            let last = fixed.pick(&LAST);
            let company = (!WITHOUT_COMPANY.contains(&id.as_str())).then(|| Company {
                name: format!("{} {}", fixed.pick(&COMPANIES), n % 97),
                size: 10 + fixed.below(5_000),
            });
            Customer {
                email: format!(
                    "{}.{}@{}.test",
                    first.to_lowercase(),
                    last.to_lowercase(),
                    n
                ),
                name: format!("{first} {last}"),
                company,
                tier: match n % 5 {
                    0 | 3 => "enterprise",
                    1 => "pro",
                    _ => "free",
                }
                .to_owned(),
                region: ["amer", "emea", "apac"][(n % 3) as usize].to_owned(),
                lifetime_value_cents: 1_000 * (50 + fixed.below(9_000)),
                id,
            }
        })
        .collect()
}

pub fn orders() -> Vec<Order> {
    let mut fixed = Fixed::new(29);
    (501..=640u64)
        .map(|n| Order {
            id: format!("O-{n}"),
            customer_id: format!("C-{}", 1001 + fixed.below(40)),
            total_cents: 500 + 25 * fixed.below(4_000),
            status: if n % 7 == 0 { "pending" } else { "paid" }.to_owned(),
        })
        .filter(|order| !MISSING_ORDERS.contains(&order.id.as_str()))
        .collect()
}

#[derive(Debug)]
struct State {
    customers: Vec<Customer>,
    orders: Vec<Order>,
    refunds: Vec<Refund>,
}

#[derive(Debug)]
pub struct Crm {
    state: Mutex<State>,
}

impl Default for Crm {
    fn default() -> Self {
        Self::new()
    }
}

impl Crm {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                customers: customers(),
                orders: orders(),
                refunds: Vec::new(),
            }),
        }
    }
}

impl Service for Crm {
    fn name(&self) -> &'static str {
        "crm"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec::new(
                "get_customer",
                "Get a customer by id (C-1001…): name, email, company (null for individuals), tier (free, pro, enterprise), region (amer, emea, apac), lifetime_value_cents. Fails when there is no such customer.",
                object_schema(json!({ "id": { "type": "string" } }), &["id"]),
            ),
            ToolSpec::new(
                "search_customers",
                "List customers, 25 per page, optionally by tier and region. Returns { customers, next_cursor? }.",
                object_schema(
                    json!({
                        "tier": { "type": "string", "enum": ["free", "pro", "enterprise"] },
                        "region": { "type": "string", "enum": ["amer", "emea", "apac"] },
                        "cursor": { "type": "string" }
                    }),
                    &[],
                ),
            ),
            ToolSpec::new(
                "get_order",
                "Get an order by id (O-501…): customer_id, total_cents, status. Fails when there is no such order.",
                object_schema(json!({ "id": { "type": "string" } }), &["id"]),
            ),
            ToolSpec::new(
                "list_orders",
                "The orders of a customer.",
                object_schema(
                    json!({ "customer_id": { "type": "string" } }),
                    &["customer_id"],
                ),
            ),
            ToolSpec::new(
                "refund_order",
                "Refund an order in full. Fails if it was already refunded.",
                object_schema(
                    json!({ "order_id": { "type": "string" }, "reason": { "type": "string" } }),
                    &["order_id", "reason"],
                ),
            ),
        ]
    }

    fn call(&self, context: &CallContext, tool: &str, args: &Map<String, Value>) -> Reply {
        let args = Args(args);
        let mut state = lock(&self.state);
        match tool {
            "get_customer" => {
                let id = match args.str("id") {
                    Ok(id) => id,
                    Err(reply) => return reply,
                };
                match state.customers.iter().find(|customer| customer.id == id) {
                    Some(customer) => Reply::Json(json!(customer)),
                    None => Reply::error(format!("not_found: no customer {id}")),
                }
            }
            "search_customers" => {
                let tier = args.opt_str("tier");
                let region = args.opt_str("region");
                let selected: Vec<&Customer> = state
                    .customers
                    .iter()
                    .filter(|customer| tier.is_none_or(|tier| customer.tier == tier))
                    .filter(|customer| region.is_none_or(|region| customer.region == region))
                    .collect();
                match page(&selected, args.opt_str("cursor"), 25) {
                    Ok((customers, next)) => {
                        let mut result = json!({ "customers": customers });
                        if let Some(next) = next {
                            result["next_cursor"] = json!(next);
                        }
                        Reply::Json(result)
                    }
                    Err(reply) => reply,
                }
            }
            "get_order" => {
                let id = match args.str("id") {
                    Ok(id) => id,
                    Err(reply) => return reply,
                };
                match state.orders.iter().find(|order| order.id == id) {
                    Some(order) => Reply::Json(json!(order)),
                    None => Reply::error(format!("not_found: no order {id}")),
                }
            }
            "list_orders" => {
                let id = match args.str("customer_id") {
                    Ok(id) => id,
                    Err(reply) => return reply,
                };
                let listed: Vec<&Order> = state
                    .orders
                    .iter()
                    .filter(|order| order.customer_id == id)
                    .collect();
                Reply::Json(json!(listed))
            }
            "refund_order" => {
                let (order_id, reason) = match (args.str("order_id"), args.str("reason")) {
                    (Ok(order_id), Ok(reason)) => (order_id.to_owned(), reason.to_owned()),
                    (Err(reply), _) | (_, Err(reply)) => return reply,
                };
                let Some(order) = state.orders.iter().find(|order| order.id == order_id) else {
                    return Reply::error(format!("not_found: no order {order_id}"));
                };
                let amount_cents = order.total_cents;
                if state
                    .refunds
                    .iter()
                    .any(|refund| refund.order_id == order_id)
                {
                    return Reply::error(format!("already_refunded: {order_id} was refunded"));
                }
                let refund = Refund {
                    refund_id: format!("R-{}", state.refunds.len() + 1),
                    order_id,
                    amount_cents,
                    reason,
                    at: rfc3339(context.now),
                };
                state.refunds.push(refund.clone());
                Reply::Json(json!(refund))
            }
            other => Reply::error(format!("unknown tool {other}")),
        }
    }

    fn snapshot(&self) -> Value {
        let state = lock(&self.state);
        let refunds: BTreeMap<&str, &Refund> = state
            .refunds
            .iter()
            .map(|refund| (refund.refund_id.as_str(), refund))
            .collect();
        json!({ "refunds": refunds })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::T0;

    #[test]
    fn fixtures_have_their_gaps() {
        let customers = customers();
        assert_eq!(customers.len(), 40);
        assert!(customers.iter().filter(|c| c.company.is_none()).count() == 3);
        let orders = orders();
        assert_eq!(orders.len(), 140 - 3);
        assert!(!orders.iter().any(|order| order.id == "O-504"));
    }

    #[test]
    fn an_order_is_refunded_once() {
        let crm = Crm::new();
        let context = CallContext {
            now: T0,
            idempotency_key: None,
        };
        let args = json!({ "order_id": "O-601", "reason": "dispute" });
        assert!(matches!(
            crm.call(&context, "refund_order", args.as_object().unwrap()),
            Reply::Json(_)
        ));
        assert!(matches!(
            crm.call(&context, "refund_order", args.as_object().unwrap()),
            Reply::Error { .. }
        ));
        assert_eq!(crm.snapshot()["refunds"]["R-1"]["order_id"], "O-601");
    }
}
