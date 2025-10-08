pub fn calculate_asap(pick_up: &str, drop_off: &str) -> (f64, u32) {
//use both pick_up and drop_off in distance calculation
    let distance = (pick_up.len() as f64 - drop_off.len() as f64).abs(); 
    let price = 5.0 + distance * 2.0;
    let eta = 15; // minutes
    (price, eta)
}

pub fn calculate_express(pick_up: &str, drop_off: &str) -> (f64, u32) {
    (8.0, 10)
}
