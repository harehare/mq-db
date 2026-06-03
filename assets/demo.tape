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

# Index Markdown files
Type "mq-db index assets/demo.md --output /tmp/mq-db-demo.mq-db"
Sleep 500ms
Enter
Sleep 1.5s

# List indexed documents
Type "mq-db list --db /tmp/mq-db-demo.mq-db"
Sleep 500ms
Enter
Sleep 1.5s

# SQL query: block type distribution
Type `mq-db sql "SELECT block_type, count(*) FROM blocks GROUP BY block_type" --db /tmp/mq-db-demo.mq-db`
Sleep 500ms
Enter
Sleep 2s

# SQL query: all headings
Type `mq-db sql "SELECT content FROM blocks WHERE block_type = 'heading'" --db /tmp/mq-db-demo.mq-db`
Sleep 500ms
Enter
Sleep 2s

# Stats
Type "mq-db stats --db /tmp/mq-db-demo.mq-db"
Sleep 500ms
Enter
Sleep 2.5s

# TUI
Type "mq-db tui --db /tmp/mq-db-demo.mq-db"
Sleep 500ms
Enter
Sleep 2s

# Focus query input
Type "i"
Sleep 0.8s

# Type a SQL query
Type `SELECT block_type, count(*) FROM blocks GROUP BY block_type`
Sleep 1s
Enter
Sleep 2s

# Navigate results
Type "j"
Sleep 0.5s
Type "j"
Sleep 0.5s
Type "k"
Sleep 0.5s

# Switch to mq mode
Tab
Sleep 1s

# Type a mq query
Type "i"
Sleep 0.5s
Type ".h1"
Sleep 1s
Enter
Sleep 1.5s

# Quit
Type "q"
Sleep 1s
