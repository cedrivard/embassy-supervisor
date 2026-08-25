use embassy_supervisor::topo_sort_const;

fn pos<const N: usize>(order: &[u8; N], x: u8) -> usize {
    order.iter().position(|&y| y == x).expect("index present")
}

#[test]
fn linear_chain_orders_deps_before_dependents() {
    const DEPS: [&[u8]; 3] = [&[], &[0], &[1]];
    const ORDER: [u8; 3] = topo_sort_const(&DEPS);
    assert_eq!(ORDER, [0, 1, 2]);
}

#[test]
fn diamond_puts_root_first_and_join_last() {
    const DEPS: [&[u8]; 4] = [&[], &[0], &[0], &[1, 2]];
    const ORDER: [u8; 4] = topo_sort_const(&DEPS);
    assert_eq!(ORDER[0], 0, "root first");
    assert_eq!(ORDER[3], 3, "join last");
    assert!(pos(&ORDER, 1) < pos(&ORDER, 3), "B before D");
    assert!(pos(&ORDER, 2) < pos(&ORDER, 3), "C before D");
}

#[test]
fn independent_nodes_all_present() {
    const DEPS: [&[u8]; 2] = [&[], &[]];
    const ORDER: [u8; 2] = topo_sort_const(&DEPS);
    assert!(ORDER.contains(&0) && ORDER.contains(&1));
}

#[test]
fn unsorted_input_is_sorted() {
    const DEPS: [&[u8]; 3] = [&[1, 2], &[], &[1]];
    const ORDER: [u8; 3] = topo_sort_const(&DEPS);
    assert!(pos(&ORDER, 1) < pos(&ORDER, 2), "1 before 2");
    assert!(pos(&ORDER, 2) < pos(&ORDER, 0), "2 before 0");
}

#[test]
fn evaluates_at_compile_time() {
    const DEPS: [&[u8]; 3] = [&[], &[0], &[1]];
    const _: () = {
        let order = topo_sort_const(&DEPS);
        assert!(order[0] == 0 && order[1] == 1 && order[2] == 2);
    };
}
