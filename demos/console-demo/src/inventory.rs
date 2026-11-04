//! Inventory state. Most fields are private to this module on purpose: a generated registry has to
//! reach them from code emitted next to the types, not from outside.

use crate::util::Describe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Tools,
    Parts,
    Misc,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: u32,
    pub name: String,
    pub category: Category,
    price_cents: i64,
    qty: u32,
    capacity: u32,
}

impl Item {
    pub fn price_cents(&self) -> i64 {
        self.price_cents
    }
}

impl Describe for Item {
    fn describe(&self) -> String {
        format!("#{} {} ({:?})", self.id, self.name, self.category)
    }
}

pub struct Inventory {
    items: Vec<Item>,
    next_id: u32,
    restock_size: u32,
}

impl Inventory {
    pub fn seeded() -> Self {
        let mut inventory = Self { items: Vec::new(), next_id: 1, restock_size: 5 };
        inventory.add("Hammer", 1_999, 6, Category::Tools);
        inventory.add("Wrench", 2_450, 4, Category::Tools);
        inventory.add("Bolt M6", 15, 120, Category::Parts);
        inventory.add("Washer", 5, 300, Category::Parts);
        inventory.add("Tape", 399, 10, Category::Misc);
        inventory
    }

    pub fn add(&mut self, name: &str, price_cents: i64, qty: u32, category: Category) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Item { id, name: name.to_owned(), category, price_cents, qty, capacity: qty });
        id
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Takes one unit of the item at `index`, or returns None when it is out of stock.
    pub fn sell_one(&mut self, index: usize) -> Option<Item> {
        let item = self.items.get_mut(index)?;
        if item.qty == 0 {
            return None;
        }
        item.qty -= 1;
        Some(item.clone())
    }

    /// Refills every empty item by `restock_size` and returns how many were refilled.
    pub fn restock_empty(&mut self) -> usize {
        let size = self.restock_size;
        let mut refilled = 0;
        for item in self.items.iter_mut().filter(|item| item.qty == 0) {
            item.qty = size.min(item.capacity);
            refilled += 1;
        }
        refilled
    }

    pub fn total_value_cents(&self) -> i64 {
        self.items.iter().map(|item| item.price_cents * i64::from(item.qty)).sum()
    }

    pub fn fill_ratios(&self) -> Vec<f64> {
        self.items.iter().map(|item| f64::from(item.qty) / f64::from(item.capacity.max(1))).collect()
    }
}
