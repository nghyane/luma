/// Ad-hoc benchmark for Layout + Renderer + Session hot paths. Not a unit test.
/// Run with: cargo test --release layout_bench -- --nocapture --ignored
///
/// Measures:
///   1. Cold refresh — first render, builds all caches
///   2. Warm refresh — snapshot unchanged, no-op fast path
///   3. Resize refresh — width change invalidates text caches
///   4. Streaming append — single token → one block dirty
///   5. collect_visible + window_iter — per-frame clone cost
///   6. Renderer paint + hash + ANSI render — full frame I/O path
///   7. Session save — full JSON write for large message history
///   8. Giant text block — single assistant response with 10k+ lines
///
/// Scenarios: 500, 2000, 5000 blocks with mixed content (markdown text + tool output).
#[cfg(test)]
mod bench {
    use crate::tui::block::{Block, TextBlock, ToolBlock};
    use crate::tui::layout::Layout;
    use std::time::Instant;

    const VIEW_WIDTH: usize = 120;
    const VIEW_HEIGHT: usize = 40;

    /// Build a realistic document of `n` blocks.
    /// Pattern: for each group → Gap, User, Gap, Thinking, Text, Gap, Tool x2
    fn build_blocks(n_blocks: usize) -> Vec<Block> {
        use crate::core::types::ContentBlock;
        use crate::tui::block::SkillBlock;
        use crate::tui::stream::StreamBuf;

        let mut blocks = Vec::with_capacity(n_blocks);
        let mut i = 0;
        while blocks.len() < n_blocks {
            // User message
            blocks.push(Block::Gap);
            if blocks.len() >= n_blocks {
                break;
            }
            blocks.push(Block::User(vec![ContentBlock::Text {
                text: format!("Question {i}: please explain how `tokio::select!` handles cancellation and timeouts in async Rust."),
            }]));
            if blocks.len() >= n_blocks {
                break;
            }
            blocks.push(Block::Gap);
            if blocks.len() >= n_blocks {
                break;
            }

            // Thinking
            let mut thinking = StreamBuf::new();
            thinking.feed(
                "The user is asking about cancellation.\nLet me think about the semantics.\n",
            );
            thinking.flush();
            blocks.push(Block::Thinking(thinking));
            if blocks.len() >= n_blocks {
                break;
            }

            // Assistant text — markdown with headers, code, lists
            let mut tb = TextBlock::new();
            tb.feed("# Answer\n\n");
            tb.feed("`tokio::select!` runs multiple futures concurrently and **races** them.\n\n");
            tb.feed("## Key points\n\n");
            tb.feed("- First future to complete wins\n");
            tb.feed("- Other branches are **dropped** (cancelled)\n");
            tb.feed("- Dropping is immediate, no cleanup\n\n");
            tb.feed("```rust\n");
            tb.feed("tokio::select! {\n");
            tb.feed("    res = fetch() => handle(res),\n");
            tb.feed("    _ = tokio::time::sleep(Duration::from_secs(5)) => timeout(),\n");
            tb.feed("}\n");
            tb.feed("```\n\n");
            tb.feed("See the [docs](https://docs.rs/tokio) for more.\n");
            tb.flush();
            blocks.push(Block::Text(tb));
            if blocks.len() >= n_blocks {
                break;
            }
            blocks.push(Block::Gap);
            if blocks.len() >= n_blocks {
                break;
            }

            // Tool 1 — Bash read (short output)
            let mut tool1 = ToolBlock::history("Bash", "$ cargo build");
            tool1.output = vec![
                "   Compiling luma v0.4.0-beta.6".to_owned(),
                "    Finished dev [unoptimized + debuginfo] target(s) in 2.31s".to_owned(),
            ];
            tool1.end_summary = "exit 0".to_owned();
            blocks.push(Block::Tool(tool1));
            if blocks.len() >= n_blocks {
                break;
            }

            // Tool 2 — Write diff (long content)
            let mut tool2 = ToolBlock::history("Edit", "src/tui/layout.rs");
            for k in 0..30 {
                tool2
                    .output
                    .push(format!("+    let x_{k} = compute_value({k});"));
            }
            tool2.end_summary = "+30 -0".to_owned();
            blocks.push(Block::Tool(tool2));
            if blocks.len() >= n_blocks {
                break;
            }

            // Skill marker occasionally
            if i % 10 == 0 && blocks.len() < n_blocks {
                blocks.push(Block::Skill(SkillBlock {
                    name: "SearchCode".to_owned(),
                    is_done: true,
                    end_summary: "found 3 matches".to_owned(),
                }));
            }

            i += 1;
        }
        blocks.truncate(n_blocks);
        blocks
    }

    fn estimate_bytes(blocks: &[Block]) -> usize {
        let mut total = 0;
        for b in blocks {
            total += match b {
                Block::Gap => 1,
                Block::GapLabel(s) | Block::Info(s) | Block::Error(s) | Block::Warn(s) => {
                    s.len() + 24
                }
                Block::User(c) => {
                    c.iter()
                        .map(|cb| match cb {
                            crate::core::types::ContentBlock::Text { text }
                            | crate::core::types::ContentBlock::Paste { text } => text.len(),
                            crate::core::types::ContentBlock::Image { .. } => 32,
                        })
                        .sum::<usize>()
                        + 24
                }
                Block::Thinking(s) => {
                    s.committed.iter().map(|l| l.len()).sum::<usize>() + s.partial().len() + 40
                }
                Block::Text(tb) => {
                    tb.stream.committed.iter().map(|l| l.len()).sum::<usize>()
                        + tb.stream.partial().len()
                        + 40
                }
                Block::Tool(tb) => {
                    tb.name.len()
                        + tb.summary.len()
                        + tb.end_summary.len()
                        + tb.output.iter().map(|l| l.len()).sum::<usize>()
                        + 64
                }
                Block::Skill(sb) => sb.name.len() + sb.end_summary.len() + 32,
            };
        }
        total
    }

    fn ms(d: std::time::Duration) -> f64 {
        d.as_secs_f64() * 1000.0
    }

    fn bench_scenario(n: usize) {
        let blocks = build_blocks(n);
        let src_bytes = estimate_bytes(&blocks);
        println!("\n── scenario: {n} blocks ({} KB src) ──", src_bytes / 1024);

        // 1. Cold refresh — first time, builds everything
        let mut layout = Layout::new(VIEW_WIDTH, VIEW_HEIGHT);
        let t = Instant::now();
        layout.refresh(&blocks, 0);
        let cold = t.elapsed();
        let total_lines = layout.total_lines();
        println!(
            "  cold refresh          {:>8.2} ms  ({total_lines} lines, {:.1} ns/line)",
            ms(cold),
            cold.as_nanos() as f64 / total_lines.max(1) as f64,
        );

        // 2. Warm refresh — no changes, should hit snapshot == fast path
        let runs = 20;
        let t = Instant::now();
        for _ in 0..runs {
            layout.refresh(&blocks, 0);
        }
        let warm = t.elapsed() / runs as u32;
        println!(
            "  warm refresh          {:>8.2} ms  (snapshot skip)",
            ms(warm)
        );

        // 3. Scrolled to bottom — different visible range
        let max_off = total_lines.saturating_sub(VIEW_HEIGHT);
        let t = Instant::now();
        layout.refresh(&blocks, max_off);
        let scrolled = t.elapsed();
        println!(
            "  scrolled refresh      {:>8.2} ms  (offset {max_off})",
            ms(scrolled)
        );

        // 4. Resize — width change invalidates all text caches
        let t = Instant::now();
        layout.set_size(VIEW_WIDTH - 20, VIEW_HEIGHT);
        layout.refresh(&blocks, 0);
        let resize = t.elapsed();
        println!(
            "  resize refresh        {:>8.2} ms  (width {} → {})",
            ms(resize),
            VIEW_WIDTH,
            VIEW_WIDTH - 20
        );

        // 5. Streaming append — simulate token append to a new block
        // Reset layout for a fair streaming measure.
        let mut streamed = build_blocks(n);
        let mut layout2 = Layout::new(VIEW_WIDTH, VIEW_HEIGHT);
        layout2.refresh(&streamed, 0);
        // Add a fresh Text block at the end
        streamed.push(Block::Text(TextBlock::new()));
        let text_idx = streamed.len() - 1;

        let tokens = [
            "Here ",
            "is ",
            "some ",
            "streaming ",
            "output ",
            "that ",
            "simulates ",
            "typical ",
            "LLM ",
            "token ",
            "chunks. ",
            "Newline\n",
            "Another ",
            "line ",
            "here.\n",
        ];
        let t = Instant::now();
        for _ in 0..50 {
            for tok in &tokens {
                if let Block::Text(tb) = &mut streamed[text_idx] {
                    tb.feed(tok);
                }
                let bottom = layout2.total_lines().saturating_sub(VIEW_HEIGHT);
                layout2.refresh(&streamed, bottom);
            }
        }
        let stream = t.elapsed();
        let frames = 50 * tokens.len();
        println!(
            "  streaming append      {:>8.2} ms total  ({:.2} ms/frame avg, {frames} frames)",
            ms(stream),
            ms(stream) / frames as f64,
        );

        // 6. collect_visible — per-frame clone cost
        let runs = 100;
        let t = Instant::now();
        let mut total_cloned = 0usize;
        for _ in 0..runs {
            let vis: Vec<_> = layout.window_iter(0, VIEW_HEIGHT).cloned().collect();
            total_cloned += vis.len();
        }
        let collect = t.elapsed() / runs as u32;
        println!(
            "  collect_visible       {:>8.2} µs  ({} lines/frame)",
            collect.as_nanos() as f64 / 1000.0,
            total_cloned / runs,
        );
    }

    #[test]
    #[ignore]
    fn layout_bench() {
        println!("\n=== Layout benchmark ===");
        println!("viewport: {VIEW_WIDTH}x{VIEW_HEIGHT}");

        for n in [500, 2000, 5000] {
            bench_scenario(n);
        }

        bench_renderer_path();
        bench_session_save();
        bench_giant_text_block();
        println!();
    }

    // ── renderer I/O path ──
    fn bench_renderer_path() {
        use crate::tui::text::ScreenBuffer;
        use crate::tui::theme::palette;

        println!("\n── renderer I/O path ──");

        let blocks = build_blocks(2000);
        let mut layout = Layout::new(VIEW_WIDTH, VIEW_HEIGHT);
        layout.refresh(&blocks, 0);
        let visible: Vec<_> = layout.window_iter(0, VIEW_HEIGHT).cloned().collect();

        let mut buf = ScreenBuffer::new(VIEW_WIDTH as u16, VIEW_HEIGHT as u16, palette::BG);

        // write_line for all visible lines
        let runs = 500;
        let t = Instant::now();
        for _ in 0..runs {
            buf.clear(palette::BG);
            for (j, line) in visible.iter().enumerate() {
                buf.write_line(line, j as u16, 0, VIEW_WIDTH as u16);
            }
        }
        let write = t.elapsed() / runs;
        println!(
            "  write_line full frame {:>8.2} µs  ({} lines)",
            write.as_nanos() as f64 / 1000.0,
            visible.len(),
        );

        // row_hash — diff check for all rows
        let t = Instant::now();
        for _ in 0..runs {
            for row in 0..VIEW_HEIGHT as u16 {
                let _ = std::hint::black_box(buf.row_hash(row));
            }
        }
        let hash = t.elapsed() / runs;
        println!(
            "  row_hash full frame   {:>8.2} µs  ({} rows)",
            hash.as_nanos() as f64 / 1000.0,
            VIEW_HEIGHT,
        );

        // render_row — ANSI string generation (worst case: every row dirty)
        let t = Instant::now();
        let mut total_bytes = 0usize;
        for _ in 0..runs {
            for row in 0..VIEW_HEIGHT as u16 {
                let s = buf.render_row(row);
                total_bytes += s.len();
            }
        }
        let render = t.elapsed() / runs;
        println!(
            "  render_row full frame {:>8.2} µs  ({} bytes/frame ANSI)",
            render.as_nanos() as f64 / 1000.0,
            total_bytes / runs as usize,
        );

        // End-to-end: write + hash + render (simulates full dirty frame)
        let t = Instant::now();
        for _ in 0..runs {
            buf.clear(palette::BG);
            for (j, line) in visible.iter().enumerate() {
                buf.write_line(line, j as u16, 0, VIEW_WIDTH as u16);
            }
            for row in 0..VIEW_HEIGHT as u16 {
                let _ = std::hint::black_box(buf.row_hash(row));
                let _ = std::hint::black_box(buf.render_row(row));
            }
        }
        let full = t.elapsed() / runs;
        println!(
            "  full frame (worst)    {:>8.2} µs  (write + hash + render all rows)",
            full.as_nanos() as f64 / 1000.0,
        );
    }

    // ── session save ──
    fn bench_session_save() {
        use crate::core::types::{ContentBlock, Message, Role, ToolCall, ToolCallFunction};

        println!("\n── session JSON save ──");

        fn build_messages(n_turns: usize) -> Vec<Message> {
            let mut msgs = Vec::new();
            msgs.push(Message::system("You are a helpful assistant."));
            for i in 0..n_turns {
                msgs.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: format!("Turn {i}: please help me debug this issue with my Rust code. I'm seeing a borrow checker error when I try to mutate a field while holding a reference to another field."),
                    }],
                    tool_call_id: None,
                    tool_calls: None,
                });
                msgs.push(Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "Here's the analysis. The issue is that Rust's borrow checker doesn't allow you to have both a mutable and immutable reference simultaneously. You can split the struct or use interior mutability.".repeat(3),
                    }],
                    tool_call_id: None,
                    tool_calls: Some(vec![ToolCall {
                        id: format!("tc_{i}"),
                        r#type: "function".into(),
                        function: ToolCallFunction {
                            name: "Read".into(),
                            arguments: r#"{"path":"src/foo.rs"}"#.into(),
                        },
                    }]),
                });
                msgs.push(Message::tool(
                    format!("tc_{i}"),
                    "file contents here\n".repeat(20),
                ));
            }
            msgs
        }

        for &n_turns in &[50usize, 200, 1000] {
            let messages = build_messages(n_turns);
            let total_chars: usize = messages.iter().map(|m| m.text().len()).sum();

            // Serialization only (no disk I/O)
            let runs = 20u32;
            let t = Instant::now();
            for _ in 0..runs {
                let _ = std::hint::black_box(serde_json::to_string(&messages).unwrap());
            }
            let serialize = t.elapsed() / runs;

            let json = serde_json::to_string(&messages).unwrap();
            println!(
                "  {n_turns:>4} turns  {:>6} KB JSON  serialize {:>7.2} ms  ({} messages, {} KB text)",
                json.len() / 1024,
                ms(serialize),
                messages.len(),
                total_chars / 1024,
            );
        }

        // Disk I/O once for a realistic payload
        let messages = build_messages(200);
        let json = serde_json::to_string(&messages).unwrap();
        let tmp = std::env::temp_dir().join("luma_bench_session.json");
        let runs = 10u32;
        let t = Instant::now();
        for _ in 0..runs {
            std::fs::write(&tmp, &json).unwrap();
        }
        let write = t.elapsed() / runs;
        println!(
            "   200 turns disk write {:>7.2} ms  ({} KB)",
            ms(write),
            json.len() / 1024,
        );
        let _ = std::fs::remove_file(&tmp);
    }

    // ── giant text block — single assistant reply with 10k+ lines ──
    fn bench_giant_text_block() {
        println!("\n── giant single text block ──");

        for &n_lines in &[1000usize, 5000, 20_000] {
            let mut tb = TextBlock::new();
            for i in 0..n_lines {
                if i % 50 == 0 {
                    tb.feed("## Section header\n\n");
                } else if i % 10 == 0 {
                    tb.feed("- bullet point with **emphasis** and `code`\n");
                } else {
                    tb.feed(&format!(
                        "line {i} with some inline `code` and **bold** content\n"
                    ));
                }
            }
            tb.flush();
            let block = Block::Text(tb);
            let blocks = vec![Block::Gap, block];

            let mut layout = Layout::new(VIEW_WIDTH, VIEW_HEIGHT);
            let t = Instant::now();
            layout.refresh(&blocks, 0);
            let cold = t.elapsed();
            let total = layout.total_lines();

            // scroll to bottom (should re-render for visible range near end)
            let max_off = total.saturating_sub(VIEW_HEIGHT);
            let t = Instant::now();
            layout.refresh(&blocks, max_off);
            let scroll = t.elapsed();

            // resize — invalidates the single giant text cache
            let t = Instant::now();
            layout.set_size(VIEW_WIDTH - 20, VIEW_HEIGHT);
            layout.refresh(&blocks, 0);
            let resize = t.elapsed();

            println!(
                "  {n_lines:>6} src lines → {total:>6} rendered  cold {:>7.2} ms  scroll {:>7.2} ms  resize {:>7.2} ms",
                ms(cold),
                ms(scroll),
                ms(resize),
            );
        }
    }
}
