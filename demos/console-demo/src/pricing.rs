//! Price rules, held by the app as `Box<dyn PriceRule>`: the trait-object case, where a registry
//! sees the trait and has to discover the concrete type behind each entry.

use crate::inventory::{Category, Item};

pub trait PriceRule {
    fn name(&self) -> &str;
    fn apply(&self, price_cents: i64, item: &Item) -> i64;
}

pub struct Discount {
    label: String,
    percent: u8,
}

impl Discount {
    pub fn new(label: &str, percent: u8) -> Self {
        Self { label: label.to_owned(), percent }
    }
}

impl PriceRule for Discount {
    fn name(&self) -> &str {
        &self.label
    }

    fn apply(&self, price_cents: i64, _item: &Item) -> i64 {
        price_cents * i64::from(100 - self.percent.min(100)) / 100
    }
}

pub struct CategoryMarkup {
    category: Category,
    percent: u8,
}

impl CategoryMarkup {
    pub fn new(category: Category, percent: u8) -> Self {
        Self { category, percent }
    }
}

impl PriceRule for CategoryMarkup {
    fn name(&self) -> &str {
        "category markup"
    }

    fn apply(&self, price_cents: i64, item: &Item) -> i64 {
        if item.category == self.category {
            price_cents * i64::from(100 + u16::from(self.percent)) / 100
        } else {
            price_cents
        }
    }
}
