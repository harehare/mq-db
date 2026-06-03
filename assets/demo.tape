Output "assets/demo.gif"

Require echo

Set Shell "zsh"
Set FontSize 16
Set Width 1200
Set Height 700
Set TypingSpeed 40ms
Set FontFamily "JetBrainsMonoNL Nerd Font Mono"
Set Padding 16
Set WindowBar Colorful
Set Framerate 20

Hide
Type "export PATH=$PATH:/Users/harehare/git/mq-db/target/release"
Enter
Type "cd /Users/harehare/git/mq-db"
Enter
Type "clear"
Enter
Show

# Index multiple Markdown files at once
Type "mq-db index assets/demo.md assets/query-guide.md assets/architecture.md --output /tmp/demo.mq-db"
Sleep 500ms
Enter
Sleep 2s

# List indexed documents
Type "mq-db list --db /tmp/demo.mq-db"
Sleep 500ms
Enter
Sleep 2s

# SQL: block count per document (cross-file)
Type `mq-db sql "SELECT d.path, count(*) AS blocks FROM blocks b JOIN documents d ON d.id = b.document_id GROUP BY d.path" --db /tmp/demo.mq-db`
Sleep 500ms
Enter
Sleep 2.5s

# SQL: H1 headings across all documents
Type `mq-db sql "SELECT d.path, b.content FROM blocks b JOIN documents d ON d.id = b.document_id WHERE b.block_type = 'heading' AND b.depth = 1" --db /tmp/demo.mq-db`
Sleep 500ms
Enter
Sleep 2.5s

# Stats across all documents
Type "mq-db stats --db /tmp/demo.mq-db"
Sleep 500ms
Enter
Sleep 2.5s

# TUI: interactive exploration
Type "mq-db tui --db /tmp/demo.mq-db"
Sleep 500ms
Enter
Sleep 2s

# Focus query input and run a SQL query
Type "i"
Sleep 0.8s
Type `SELECT d.path, b.content FROM blocks b JOIN documents d ON d.id = b.document_id WHERE b.block_type = 'heading' AND b.depth = 1`
Sleep 1s
Enter
Sleep 2s

# Quit
Type "q"
Sleep 1s
