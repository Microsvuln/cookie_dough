use std::io;
use std::path::Path;
use std::process::Command;
use std::collections::{VecDeque, BTreeSet};
use rand::random;

/// Max size of all fuzz inputs
const INPUT_SIZE: usize = 256;

/// The root node is always the 0th index in the node list
const ROOT: NodeRef = NodeRef(0);

/// An index into `nodes` for a graph
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct NodeRef(pub usize);

/// A graph
pub struct Graph {
    /// Nodes in the graph
    nodes: Vec<Node>,
}

impl Graph {
    /// Create a new graph
    pub fn new() -> Self {
        // Create an empty graph
        let mut graph = Graph {
            nodes: Vec::new(),
        };

        // Add a root node
        graph.add_node();

        graph
    }

    /// Create a new random graph
    pub fn new_rand(max_nodes: usize) -> Self {
        assert!(max_nodes > 0, "Max nodes must be non-zero");

        let mut graph = Graph::new();

        for _ in 0..max_nodes - 1 {
            // Create a new node
            graph.add_node();
        }

        for ii in 0..max_nodes * 2 {
            // Randomly link nodes
            let a = if ii == 0 {
                // Always link the root node first
                ROOT.0
            } else {
                random::<usize>() % graph.nodes.len()
            };

            let b = random::<usize>() % graph.nodes.len();

            // Generate a random condition
            let condition = match random::<usize>() % 3 {
                0 => {
                    Edge::Unconditional
                }
                1 => {
                    let min = random::<u8>();
                    Edge::InputU8 {
                        idx: random::<usize>() % INPUT_SIZE,
                        min: min,
                        max: min + (random::<u16>() %
                            (std::u8::MAX as u16 - min as u16 + 1)) as u8,
                    }
                }
                2 => {
                    let min = random::<usize>() % (INPUT_SIZE + 1);
                    Edge::InputSize {
                        min: min,
                        max: min + random::<usize>() % (INPUT_SIZE + 1 - min),
                    }
                }
                _ => unreachable!(),
            };

            graph.nodes[a].edge.push((NodeRef(b), condition));
        }

        graph
    }
    
    /// Create a new random graph using only linear conditional flow (no loops)
    pub fn new_rand_cond_noloop(max_depth: usize) -> Self {
        let mut graph = Graph::new();

        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();
        queue.push_back((0, ROOT));

        // Current input byte we are consuming
        let mut cur_byte = 0;

        while let Some((depth, node)) = queue.pop_back() {
            if !visited.insert(node) { continue; }
            if depth > max_depth { continue; }

            let min = random::<u8>();
            assert!(cur_byte < INPUT_SIZE);
            let cond = Edge::InputU8 {
                idx: cur_byte,
                min: min,
                max: min + (random::<u16>() %
                    (std::u8::MAX as u16 - min as u16 + 1)) as u8,
            };
            cur_byte += 1;

            let ttgt = if random::<bool>() {
                graph.add_node()
            } else {
                NodeRef(random::<usize>() % graph.nodes.len())
            };

            let ftgt = graph.add_node();

            let node = graph.node_mut(node).unwrap();
            node.edge.push((ttgt, cond));
            node.edge.push((ftgt, Edge::Unconditional));

            // Queue up exploring the targets
            queue.push_back((depth + 1, ttgt));
            queue.push_back((depth + 1, ftgt));
        }

        graph
    }

    /// Traverse the graph in BFS order
    pub fn traverse_bfs<F>(&self, mut visitor: F)
            where F: FnMut(&Node, Option<(&Node, &Edge)>) {
        // Contains state of if we visisted a node
        let mut visited = vec![false; self.nodes.len()];

        // Queue of nodes to visit
        let mut queue = VecDeque::new();
        queue.push_back(ROOT);

        while let Some(node) = queue.pop_front() {
            // Check visited state
            if visited[node.0] { continue; }

            // Set that we visisted this node
            visited[node.0] = true;

            // Get access to the node
            let node = self.node_ref(node).unwrap();

            if node.edge.len() > 0 {
                // Queue up visiting of node edges
                for &edge in &node.edge {
                    // Invoke the visitor
                    visitor(node,
                        Some((self.node_ref(edge.0).unwrap(), &edge.1)));

                    // Make sure we visit the edge
                    queue.push_back(edge.0);
                }
            } else {
                // Node has no edges, still report it but with no `to`
                visitor(node, None);
            }
        }
    }

    /// Generate a C program with constraints requested
    pub fn generate_c<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        // Create an empty program
        let mut prog = String::new();

        assert!(self.nodes.len() <= 1024,
            "Too many nodes, update shmem->coverage_freqs");

        prog += &format!(r#"
#include <stdlib.h>
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
#include <immintrin.h>
#include <sys/mman.h>

struct _shmem {{
    uint64_t fuzz_cases;
    uint64_t coverage;
    uint64_t coverage_freqs[1024];
}};

void parser(struct _shmem *shmem, uint8_t *input, size_t input_size);

uint64_t
xorshift(void) {{
    static uint64_t seed = 0;
    if(seed == 0) seed = __rdtsc();
    seed ^= seed << 13;
    seed ^= seed >> 17;
    seed ^= seed << 43;
    return seed;
}}

int
main(int argc, char *argv[]) {{
    if(argc != 2) {{
        printf("usage: %s <input filename>\n", argc > 0 ? argv[0] : "a.out");
        return -1;
    }}
    
    uint8_t *buf = calloc(1, {INPUT_SIZE});
    size_t input_len = 0;
    if(!buf) {{
        perror("malloc() error ");
        return -1;
    }}

    int shmfd = open("shared_memory.shm", O_RDWR | O_CREAT, 0644);
    if(shmfd < 0) {{
        perror("open() error ");
        return -1;
    }}

    ftruncate(shmfd, sizeof(struct _shmem));

    struct _shmem *shm = mmap(NULL, sizeof(struct _shmem),
        PROT_READ | PROT_WRITE, MAP_SHARED, shmfd, 0);
    if(shm == MAP_FAILED) {{
        perror("mmap() error ");
        return -1;
    }}
    
    if(strcmp(argv[1], "internal")) {{
        FILE *fd = fopen(argv[1], "rb");
        if(!fd) {{
            perror("fopen() error ");
            return -1;
        }}

        ssize_t input_len = fread(buf, 1, {INPUT_SIZE}, fd);
        if(input_len < 0) {{
            perror("fread() error ");
            return -1;
        }}
    
        parser(shm, buf, (size_t)input_len);
    }} else {{
        void **inputs = malloc(sizeof(void*) * 1024);
        size_t num_inputs = 0;

        input_len = {INPUT_SIZE};

        for( ; ; ) {{
            uint64_t old_cov = shm->coverage;

            if(num_inputs > 0) {{
                // Use an existing input as the basis
                size_t sel = xorshift() % num_inputs;
                memcpy(buf, inputs[sel], {INPUT_SIZE});
            }}

            for(int ii = 0; ii < 2; ii++) {{
                size_t sel = xorshift() % {INPUT_SIZE};
                buf[sel] = xorshift();
            }}

            parser(shm, buf, (size_t)input_len);

            if(shm->coverage > old_cov) {{
                uint8_t *cloned = calloc(1, {INPUT_SIZE});
                memcpy(cloned, buf, {INPUT_SIZE});
                if(num_inputs >= 1024) __builtin_trap();
                inputs[num_inputs++] = cloned;
            }}

            usleep(100);
        }}
    }}


    return 0;
}}

void parser(struct _shmem *shm, uint8_t *input, size_t input_size) {{
    uint64_t branches = 0;

    shm->fuzz_cases += 1;
"#, INPUT_SIZE = INPUT_SIZE);

        for node in &self.nodes {
            // Generate a label for this node
            prog += &format!("node{}:\n", node.id.0);

            prog += &format!("{{    uint64_t *cov = \
                &shm->coverage_freqs[{}];\n",
                node.id.0);
            prog += "    if(*cov == 0) shm->coverage++;\n";
            prog += "    *cov = *cov + 1;}\n";

            // Emit conditionals
            for (tgt, cond) in &node.edge {
                macro_rules! branch {
                    () => {
                        prog += "    if(branches++ > 1000) return;\n";
                        prog += &format!("    goto node{};\n", tgt.0);
                    }
                }

                prog += "{\n";

                match cond {
                    Edge::Unconditional => {
                        branch!();
                    }
                    Edge::InputU8 { idx, min, max } => {
                        prog += &format!("    if(input[{}] >= {:#x} && \
                            input[{}] <= {:#x}) {{\n", idx, min, idx, max);
                        branch!();
                        prog += "    }\n";
                    }
                    Edge::InputSize { min, max } => {
                        prog += &format!("    if(input_size >= {:#x} && \
                            input_size <= {:#x}) {{\n", min, max);
                        branch!();
                        prog += "    }\n";
                    }
                }
            
                prog += "}\n";
            }

            // No explicit fallthrough, just return out
            prog += "return;\n";
        }

        prog += "}\n";

        // Write out the program
        std::fs::write(path, prog)?;

        Ok(())
    }

    /// Dump the graph to a dotfile with name `path.dot` and then invoke
    /// GraphViz to convert it to a SVG file at `path`
    pub fn dump_svg<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        // Create the dot file name
        let dotfn = path.as_ref().with_extension("dot");

        // Construct the DOT file
        let mut dot = String::new();
        dot += "digraph {\n";
      
        // First, create the HTML IDs for all blocks
        let mut reported = BTreeSet::new();
        self.traverse_bfs(|from, _to| {
            if !reported.insert(from.id) { return; }
            dot += &format!("    \"{}\" [id=\"node{}\"];\n", from.name,
                from.id.0);
        });

        // Create edges
        self.traverse_bfs(|from, to| {
            if let Some((to, cond)) = to {
                // Construct the condition string
                let condstr = match cond {
                    Edge::Unconditional => {
                        "UC".to_string()
                    }
                    Edge::InputU8 { idx, min, max } => {
                        format!("U8 @ {}\n[{:#x}, {:#x}]",
                            idx, min, max)
                    }
                    Edge::InputSize { min, max } => {
                        format!("ISZ\n[{:#x}, {:#x}]", min, max)
                    }
                };

                dot += &format!("    \"{}\" -> \"{}\" [label=\"{}\"];\n",
                    from.name, to.name, condstr);
            }
        });
        
        dot += "}\n";

        // Write out the DOT file
        std::fs::write(&dotfn, dot)?;

        // Convert the DOT file to an SVG
        let status = Command::new("dot").args(&[
            "-Tsvg",
            "-o", path.as_ref().to_str().unwrap(),
            dotfn.to_str().unwrap(),
        ]).status()?;
        assert!(status.success(), "DOT failed with error");

        Ok(())
    }

    /// Add a node to the graph (unlinked)
    pub fn add_node(&mut self) -> NodeRef {
        // Get the node ID for the node we're about to add
        let node_id = self.nodes.len();

        // Create a node and set a generic name for the node
        let node = Node {
            name: format!("{:?}", NodeRef(node_id)),
            id:   NodeRef(node_id),
            edge: Vec::new(),
        };

        // Add a default node
        self.nodes.push(node);

        // Return the reference to the node
        NodeRef(node_id)
    }
    
    /// Get an immutable reference to a node
    #[inline]
    pub fn node_ref(&self, id: NodeRef) -> Option<&Node> {
        self.nodes.get(id.0)
    }

    /// Get a mutable reference to a node
    #[inline]
    pub fn node_mut(&mut self, id: NodeRef) -> Option<&mut Node> {
        self.nodes.get_mut(id.0)
    }
}

/// A node in the graph
#[derive(Debug)]
pub struct Node {
    /// User-friendly name for the node
    name: String,

    /// ID of the node itself
    id: NodeRef,

    /// Edges for the node
    edge: Vec<(NodeRef, Edge)>,
}

/// A conditional edge
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Edge {
    /// Always branch
    Unconditional,

    /// Branch if the `u8` contained at `idx` from the input was in the range
    /// of `min` <= `u8` <= `max`
    InputU8 {
        idx: usize,
        min: u8,
        max: u8,
    },
    
    /// Branch if the input size falls in `min` <= input size <= `max`
    InputSize {
        min: usize,
        max: usize,
    },
}

fn main() {
    let graph = Graph::new_rand_cond_noloop(12);
    graph.dump_svg("../coverage_server/foo.svg").unwrap();
    graph.generate_c("../afl_test/foo.c").unwrap();
}

