use gc::{Finalize, Gc, GcCell, Trace, force_collect};

/// 1. Basic usage of Trace and Finalize traits.
///    Most types can derive Trace and Finalize automatically if all their fields are also Trace.
#[derive(Trace, Finalize)]
struct SimpleData {
    id: i32,
    name: String,
}

/// 2. Implementing a linked list with cycles.
///    In standard Rust, cycles with Rc/Arc would lead to memory leaks.
///    With Gc, we can easily handle cycles.
#[derive(Trace, Finalize)]
struct Node {
    value: i32,
    // GcCell allows for mutation inside a Gc pointer.
    next: GcCell<Option<Gc<Node>>>,
}

/// 3. Demonstrating custom Finalize logic.
struct DatabaseConnection {
    id: String,
}

impl Drop for DatabaseConnection {
    fn drop(&mut self) {
        log::trace!("Dropping DatabaseConnection with id: {}", self.id);
    }
}

impl Finalize for DatabaseConnection {
    fn finalize(&self) {
        log::trace!("Closing database connection: {}", self.id);
    }
}

// Since DatabaseConnection doesn't contain any Gc pointers, we can use unsafe_empty_trace.
unsafe impl Trace for DatabaseConnection {
    gc::unsafe_empty_trace!();
}

fn main() {
    env_logger::init();

    println!("--- GC Library Comprehensive Example ---");

    // --- Basic Allocation ---
    {
        let data = Gc::new(SimpleData {
            id: 1,
            name: "First Data".to_string(),
        });
        println!("Allocated: id={}, name={}", data.id, data.name);
    } // 'data' goes out of scope here. It's now eligible for collection.

    // --- Cycles and Mutability ---
    println!("\n--- Cycles and GcCell ---");
    {
        let node1 = Gc::new(Node {
            value: 10,
            next: GcCell::new(None),
        });
        let node2 = Gc::new(Node {
            value: 20,
            next: GcCell::new(None),
        });

        // Create a cycle: node1 -> node2 -> node1
        *node1.next.borrow_mut() = Some(node2.clone());
        *node2.next.borrow_mut() = Some(node1.clone());

        println!(
            "Created a cycle between node 1 (val: {}) and node 2 (val: {})",
            node1.value, node2.value
        );
    } // node1 and node2 go out of scope. In Rc/Arc, this would leak.

    // Explicitly trigger collection to show that the cycle is cleaned up.
    println!("Triggering GC collection for cycles...");
    force_collect();

    // --- Finalization ---
    println!("\n--- Finalization ---");
    {
        let _conn = Gc::new(DatabaseConnection {
            id: "conn_001".to_string(),
        });
        println!("Database connection created.");
    }

    println!("Triggering GC collection for finalization...");
    // Finalization happens during collection.
    force_collect();

    // --- Weak Pointers ---
    println!("\n--- Weak Pointers ---");
    {
        let data = Gc::new(100);
        let weak_data = data.clone_weak_gc();

        // Use .value() to access the data if it's still alive.
        if let Some(val) = weak_data.value() {
            println!("Weak pointer value: {}", *val);
        }

        drop(data);
        // Force collection to ensure the Gc object is gone.
        force_collect();

        if weak_data.value().is_none() {
            println!("Weak pointer correctly invalidated after GC collection.");
        } else {
            println!(
                "Weak pointer still valid (this might happen if GC didn't run or didn't collect yet)."
            );
        }
    }

    println!("\n--- Complex Data Structures ---");
    {
        // Gc can be nested in Vec, HashMap, etc., as long as they are traceable.
        let list = Gc::new(vec![Gc::new(1), Gc::new(2), Gc::new(3)]);
        println!(
            "Managed list of Gc pointers: {:?}",
            list.iter().map(|x| **x).collect::<Vec<_>>()
        );
    }

    // --- Ephemerons (WeakPair) ---
    println!("\n--- Ephemerons (WeakPair) ---");
    {
        let key = Gc::new("key_data".to_string());
        let pair = key.create_weak_pair("value_data".to_string());

        println!("WeakPair key: {:?}", pair.key());
        println!("WeakPair value: {:?}", pair.value());

        drop(key);
        println!("Key dropped. Triggering GC...");
        force_collect();

        // Since the key is gone, the value in WeakPair should also be gone (or inaccessible via pair).
        println!("WeakPair key after GC: {:?}", pair.key());
        println!("WeakPair value after GC: {:?}", pair.value());
    }

    println!("\nExample finished successfully.");
}
