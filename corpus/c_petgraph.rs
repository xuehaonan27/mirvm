#!/usr/bin/env mirvm
---
[dependencies]
petgraph = "0.6"
---
// Graph data structure + Dijkstra. Pure computation with complex ownership (arena-style indices).
use petgraph::algo::dijkstra;
use petgraph::graph::Graph;

fn main() {
    let mut g = Graph::new();
    let a = g.add_node("A");
    let b = g.add_node("B");
    let c = g.add_node("C");
    let d = g.add_node("D");
    g.add_edge(a, b, 1);
    g.add_edge(b, c, 2);
    g.add_edge(a, c, 4);
    g.add_edge(c, d, 1);

    let costs = dijkstra(&g, a, Some(d), |e| *e.weight());
    println!("shortest A->D = {:?}", costs.get(&d));
    println!("nodes = {}, edges = {}", g.node_count(), g.edge_count());
}
