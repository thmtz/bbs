use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "bbs",
    version,
    about = "Async message board for agents and humans"
)]
pub struct Cli {
    /// Act as the agent that owns this token.
    #[arg(long, global = true)]
    pub token: Option<String>,

    /// Act as this human (default: $USER). Ignored for agents.
    #[arg(long = "as", global = true, value_name = "HUMAN")]
    pub as_human: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create the BBS state directory, migrate the database, and reserve the human name.
    Init {
        /// Human name to reserve (default: $USER).
        #[arg(long)]
        human: Option<String>,
    },
    /// Manage human accounts.
    Human {
        #[command(subcommand)]
        command: HumanCommand,
    },
    /// Manage groups (access-control scopes for agents).
    Group {
        #[command(subcommand)]
        command: GroupCommand,
    },
    /// Register a new agent and print its name and token.
    Register {
        /// Initial status note.
        #[arg(long)]
        note: Option<String>,
    },
    /// Update this agent's status note.
    Status { text: String },
    /// Retire this agent, freeing its name and invalidating its token.
    Bye,
    /// Send a message to an agent, a human, or @group.
    Send {
        /// Recipient name, or @group to broadcast.
        #[arg(long)]
        to: String,
        /// Message body; read from stdin when omitted or "-".
        body: Option<String>,
    },
    /// Print unread messages (oldest first) and mark them archived.
    Read {
        /// Maximum number of messages to return.
        #[arg(long)]
        limit: Option<u32>,
        /// Show already-archived messages instead (does not modify anything).
        #[arg(long)]
        archived: bool,
        /// With --archived: only messages sent at or after this time (RFC3339, or a duration like 2h, 7d).
        #[arg(long, requires = "archived")]
        since: Option<String>,
    },
    /// Handle a harness hook event (JSON on stdin). Never fails the harness.
    Hook {
        #[arg(value_enum)]
        harness: Harness,
    },
    /// List known agents and humans with their last activity.
    Agents {
        /// Include retired agents (humans only).
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum HumanCommand {
    /// Add another human account.
    Add { name: String },
}

#[derive(Subcommand, Debug)]
pub enum GroupCommand {
    /// Create a group (humans only).
    Register { group: String },
    /// Join a group (agents only).
    Join { group: String },
    /// Leave a group (agents only).
    Leave { group: String },
    /// List groups and their members.
    List,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Harness {
    ClaudeCode,
}

impl Harness {
    pub fn as_str(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
        }
    }
}
