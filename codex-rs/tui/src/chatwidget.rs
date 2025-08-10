use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use codex_core::codex_wrapper::CodexConversation;
use codex_core::codex_wrapper::init_codex;
use codex_core::config::Config;
use codex_core::config_types::ReasoningEffort;
use codex_core::protocol::AgentMessageDeltaEvent;
use codex_core::protocol::AgentMessageEvent;
use codex_core::protocol::AgentReasoningDeltaEvent;
use codex_core::protocol::AgentReasoningEvent;
use codex_core::protocol::AgentReasoningRawContentDeltaEvent;
use codex_core::protocol::AgentReasoningRawContentEvent;
use codex_core::protocol::ApplyPatchApprovalRequestEvent;
use codex_core::protocol::BackgroundEventEvent;
use codex_core::protocol::ErrorEvent;
use codex_core::protocol::Event;
use codex_core::protocol::EventMsg;
use codex_core::protocol::ExecApprovalRequestEvent;
use codex_core::protocol::ExecCommandBeginEvent;
use codex_core::protocol::ExecCommandEndEvent;
use codex_core::protocol::InputItem;
use codex_core::protocol::McpToolCallBeginEvent;
use codex_core::protocol::McpToolCallEndEvent;
use codex_core::protocol::Op;
use codex_core::protocol::PatchApplyBeginEvent;
use codex_core::protocol::TaskCompleteEvent;
use codex_core::protocol::TokenUsage;
use codex_core::protocol::TurnDiffEvent;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use tracing::info;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::BottomPane;
use crate::bottom_pane::BottomPaneParams;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::InputResult;
use crate::history_cell::CommandOutput;
use crate::history_cell::HistoryCell;
use crate::history_cell::PatchEventType;
use crate::live_wrap::RowBuilder;
use crate::text_block::TextBlock;
use crate::user_approval_widget::ApprovalRequest;
use codex_browser::{BrowserManager, config::BrowserConfig};
use codex_file_search::FileMatch;
use ratatui::style::Stylize;

struct RunningCommand {
    command: Vec<String>,
    #[allow(dead_code)]
    cwd: PathBuf,
}

pub(crate) struct ChatWidget<'a> {
    app_event_tx: AppEventSender,
    codex_op_tx: UnboundedSender<Op>,
    bottom_pane: BottomPane<'a>,
    active_history_cell: Option<HistoryCell>,
    config: Config,
    initial_user_message: Option<UserMessage>,
    total_token_usage: TokenUsage,
    last_token_usage: TokenUsage,
    reasoning_buffer: String,
    content_buffer: String,
    // Buffer for streaming assistant answer text; we do not surface partial
    // We wait for the final AgentMessage event and then emit the full text
    // at once into scrollback so the history contains a single message.
    answer_buffer: String,
    running_commands: HashMap<String, RunningCommand>,
    live_builder: RowBuilder,
    current_stream: Option<StreamKind>,
    stream_header_emitted: bool,
    live_max_rows: u16,
    // Store pending image paths keyed by their placeholder text
    pending_images: HashMap<String, PathBuf>,
    welcome_shown: bool,
    // Browser manager for screenshot functionality
    browser_manager: Arc<BrowserManager>,
}

struct UserMessage {
    text: String,
    image_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Answer,
    Reasoning,
}

impl From<String> for UserMessage {
    fn from(text: String) -> Self {
        Self {
            text,
            image_paths: Vec::new(),
        }
    }
}

fn create_initial_user_message(text: String, image_paths: Vec<PathBuf>) -> Option<UserMessage> {
    if text.is_empty() && image_paths.is_empty() {
        None
    } else {
        Some(UserMessage { text, image_paths })
    }
}

impl ChatWidget<'_> {
    fn parse_message_with_images(&mut self, text: String) -> UserMessage {
        use std::path::Path;
        
        // Common image extensions
        const IMAGE_EXTENSIONS: &[&str] = &[
            ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".svg", ".ico", ".tiff", ".tif"
        ];
        
        let mut image_paths = Vec::new();
        let mut cleaned_text = text.clone();
        
        // First, handle [image: ...] placeholders from drag-and-drop
        let placeholder_regex = regex_lite::Regex::new(r"\[image: [^\]]+\]").unwrap();
        for mat in placeholder_regex.find_iter(&text) {
            let placeholder = mat.as_str();
            if let Some(path) = self.pending_images.remove(placeholder) {
                image_paths.push(path);
                // Remove the placeholder from the text
                cleaned_text = cleaned_text.replace(placeholder, "");
            }
        }
        
        // Then check for direct file paths in the text
        let words: Vec<String> = text.split_whitespace().map(String::from).collect();
        
        for word in &words {
            // Skip placeholders we already handled
            if word.starts_with("[image:") {
                continue;
            }
            
            // Check if this looks like an image path
            let is_image_path = IMAGE_EXTENSIONS.iter().any(|ext| word.to_lowercase().ends_with(ext));
            
            if is_image_path {
                let path = Path::new(word);
                
                // Check if it's a relative or absolute path that exists
                if path.exists() {
                    image_paths.push(path.to_path_buf());
                    // Remove the path from the text
                    cleaned_text = cleaned_text.replace(word, "");
                } else {
                    // Try with common relative paths
                    let potential_paths = vec![
                        PathBuf::from(word),
                        PathBuf::from("./").join(word),
                        std::env::current_dir().ok().map(|d| d.join(word)).unwrap_or_default(),
                    ];
                    
                    for potential_path in potential_paths {
                        if potential_path.exists() {
                            image_paths.push(potential_path);
                            cleaned_text = cleaned_text.replace(word, "");
                            break;
                        }
                    }
                }
            }
        }
        
        // Clean up extra whitespace
        cleaned_text = cleaned_text.split_whitespace().collect::<Vec<_>>().join(" ");
        
        UserMessage {
            text: cleaned_text,
            image_paths,
        }
    }
    
    fn interrupt_running_task(&mut self) {
        if self.bottom_pane.is_task_running() {
            self.active_history_cell = None;
            self.bottom_pane.clear_ctrl_c_quit_hint();
            self.submit_op(Op::Interrupt);
            self.bottom_pane.set_task_running(false);
            self.bottom_pane.clear_live_ring();
            self.live_builder = RowBuilder::new(self.live_builder.width());
            self.current_stream = None;
            self.stream_header_emitted = false;
            self.answer_buffer.clear();
            self.reasoning_buffer.clear();
            self.content_buffer.clear();
            self.request_redraw();
        }
    }
    fn layout_areas(&self, area: Rect) -> [Rect; 2] {
        Layout::vertical([
            Constraint::Max(
                self.active_history_cell
                    .as_ref()
                    .map_or(0, |c| c.desired_height(area.width)),
            ),
            Constraint::Min(self.bottom_pane.desired_height(area.width)),
        ])
        .areas(area)
    }
    fn emit_stream_header(&mut self, kind: StreamKind) {
        use ratatui::text::Line as RLine;
        if self.stream_header_emitted {
            return;
        }
        let header = match kind {
            StreamKind::Reasoning => RLine::from("thinking".magenta().italic()),
            StreamKind::Answer => RLine::from("codex".magenta().bold()),
        };
        self.app_event_tx
            .send(AppEvent::InsertHistory(vec![header]));
        self.stream_header_emitted = true;
    }
    fn finalize_active_stream(&mut self) {
        if let Some(kind) = self.current_stream {
            self.finalize_stream(kind);
        }
    }
    pub(crate) fn new(
        config: Config,
        app_event_tx: AppEventSender,
        initial_prompt: Option<String>,
        initial_images: Vec<PathBuf>,
        enhanced_keys_supported: bool,
    ) -> Self {
        let (codex_op_tx, mut codex_op_rx) = unbounded_channel::<Op>();

        let app_event_tx_clone = app_event_tx.clone();
        // Create the Codex asynchronously so the UI loads as quickly as possible.
        let config_for_agent_loop = config.clone();
        tokio::spawn(async move {
            let CodexConversation {
                codex,
                session_configured,
                ..
            } = match init_codex(config_for_agent_loop).await {
                Ok(vals) => vals,
                Err(e) => {
                    // TODO: surface this error to the user.
                    tracing::error!("failed to initialize codex: {e}");
                    return;
                }
            };

            // Forward the captured `SessionInitialized` event that was consumed
            // inside `init_codex()` so it can be rendered in the UI.
            app_event_tx_clone.send(AppEvent::CodexEvent(session_configured.clone()));
            let codex = Arc::new(codex);
            let codex_clone = codex.clone();
            tokio::spawn(async move {
                while let Some(op) = codex_op_rx.recv().await {
                    let id = codex_clone.submit(op).await;
                    if let Err(e) = id {
                        tracing::error!("failed to submit op: {e}");
                    }
                }
            });

            while let Ok(event) = codex.next_event().await {
                app_event_tx_clone.send(AppEvent::CodexEvent(event));
            }
        });

        // Browser manager will be initialized lazily when needed
        // For now, create a placeholder that will be replaced on first use
        let browser_config = BrowserConfig::default();
        let browser_manager = Arc::new(BrowserManager::new(browser_config));
        
        Self {
            app_event_tx: app_event_tx.clone(),
            codex_op_tx,
            bottom_pane: BottomPane::new(BottomPaneParams {
                app_event_tx,
                has_input_focus: true,
                enhanced_keys_supported,
            }),
            active_history_cell: None,
            config,
            initial_user_message: create_initial_user_message(
                initial_prompt.unwrap_or_default(),
                initial_images,
            ),
            total_token_usage: TokenUsage::default(),
            last_token_usage: TokenUsage::default(),
            reasoning_buffer: String::new(),
            content_buffer: String::new(),
            answer_buffer: String::new(),
            running_commands: HashMap::new(),
            live_builder: RowBuilder::new(80),
            current_stream: None,
            stream_header_emitted: false,
            live_max_rows: 3,
            pending_images: HashMap::new(),
            welcome_shown: false,
            browser_manager,
        }
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        self.bottom_pane.desired_height(width)
            + self
                .active_history_cell
                .as_ref()
                .map_or(0, |c| c.desired_height(width))
    }

    pub(crate) fn handle_key_event(&mut self, key_event: KeyEvent) {
        if key_event.kind == KeyEventKind::Press {
            self.bottom_pane.clear_ctrl_c_quit_hint();
        }

        match self.bottom_pane.handle_key_event(key_event) {
            InputResult::Submitted(text) => {
                let user_message = self.parse_message_with_images(text);
                self.submit_user_message(user_message);
            }
            InputResult::None => {}
        }
    }

    pub(crate) fn handle_paste(&mut self, text: String) {
        // Check if the pasted text is a file path to an image
        let trimmed = text.trim();
        
        tracing::info!("Paste received: {:?}", trimmed);
        
        const IMAGE_EXTENSIONS: &[&str] = &[
            ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp", ".svg", ".ico", ".tiff", ".tif"
        ];
        
        // Check if it looks like a file path
        let is_likely_path = trimmed.starts_with("file://") 
            || trimmed.starts_with("/") 
            || trimmed.starts_with("~/")
            || trimmed.starts_with("./");
            
        if is_likely_path {
            // Remove escape backslashes that terminals add for special characters
            let unescaped = trimmed
                .replace("\\ ", " ")
                .replace("\\(", "(")
                .replace("\\)", ")");
            
            // Handle file:// URLs (common when dragging from Finder)
            let path_str = if unescaped.starts_with("file://") {
                // URL decode to handle spaces and special characters
                // Simple decoding for common cases (spaces as %20, etc.)
                unescaped.strip_prefix("file://")
                    .map(|s| s.replace("%20", " ")
                             .replace("%28", "(")
                             .replace("%29", ")")
                             .replace("%5B", "[")
                             .replace("%5D", "]")
                             .replace("%2C", ",")
                             .replace("%27", "'")
                             .replace("%26", "&")
                             .replace("%23", "#")
                             .replace("%40", "@")
                             .replace("%2B", "+")
                             .replace("%3D", "=")
                             .replace("%24", "$")
                             .replace("%21", "!")
                             .replace("%2D", "-")
                             .replace("%2E", "."))
                    .unwrap_or_else(|| unescaped.clone())
            } else {
                unescaped
            };
            
            tracing::info!("Decoded path: {:?}", path_str);
            
            // Check if it has an image extension
            let is_image = IMAGE_EXTENSIONS.iter().any(|ext| path_str.to_lowercase().ends_with(ext));
            
            if is_image {
                let path = PathBuf::from(&path_str);
                tracing::info!("Checking if path exists: {:?}", path);
                if path.exists() {
                    tracing::info!("Image file dropped/pasted: {:?}", path);
                    // Get just the filename for display
                    let filename = path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("image");
                    
                    // Add a placeholder to the compose field instead of submitting
                    let placeholder = format!("[image: {}]", filename);
                    
                    // Store the image path for later submission
                    self.pending_images.insert(placeholder.clone(), path);
                    
                    // Add the placeholder text to the compose field
                    self.bottom_pane.handle_paste(placeholder);
                    return;
                } else {
                    tracing::warn!("Image path does not exist: {:?}", path);
                }
            }
        }
        
        // Otherwise handle as regular text paste
        self.bottom_pane.handle_paste(text);
    }

    fn add_to_history(&mut self, cell: HistoryCell) {
        self.app_event_tx
            .send(AppEvent::InsertHistory(cell.plain_lines()));
    }

    fn submit_user_message(&mut self, user_message: UserMessage) {
        let UserMessage { text, image_paths } = user_message;
        
        // Keep the original text for display purposes
        let original_text = text.clone();
        let mut actual_text = text;
        
        // Process slash commands and expand them if needed
        let processed = crate::slash_command::process_slash_command_message(&actual_text);
        match processed {
            crate::slash_command::ProcessedCommand::ExpandedPrompt(expanded) => {
                // Replace the slash command with the expanded prompt for the LLM
                actual_text = expanded;
            }
            crate::slash_command::ProcessedCommand::RegularCommand(cmd, _args) => {
                // This is a regular slash command, dispatch it normally
                self.app_event_tx.send(AppEvent::DispatchCommand(cmd, actual_text.clone()));
                return;
            }
            crate::slash_command::ProcessedCommand::Error(error_msg) => {
                // Show error in history
                self.add_to_history(HistoryCell::new_error_event(error_msg));
                return;
            }
            crate::slash_command::ProcessedCommand::NotCommand(_) => {
                // Not a slash command, process normally
            }
        }
        
        let mut items: Vec<InputItem> = Vec::new();
        let mut browser_screenshot_count = 0;

        // Check if browser mode is enabled and capture screenshot
        // Always check both local and global browser managers
        let local_browser_manager = self.browser_manager.clone();
        
        // Check if either local or global browser is enabled
        let local_enabled = local_browser_manager.config.try_read()
            .map(|c| c.enabled)
            .unwrap_or(false);
        
        // Check global browser manager async
        let (tx_global, rx_global) = std::sync::mpsc::channel();
        tokio::spawn(async move {
            let global_manager = codex_browser::global::get_browser_manager().await;
            let enabled = if let Some(ref mgr) = global_manager {
                mgr.is_enabled().await
            } else {
                false
            };
            let _ = tx_global.send((global_manager, enabled));
        });
        
        // Get global browser state
        let (global_manager, global_enabled) = rx_global.recv_timeout(std::time::Duration::from_millis(100))
            .unwrap_or((None, false));
        
        // Use global manager if it exists and is enabled, otherwise use local
        let (browser_manager, browser_enabled) = if global_enabled && global_manager.is_some() {
            (global_manager.unwrap(), true)
        } else if local_enabled {
            (local_browser_manager, true)
        } else if let Some(global) = global_manager {
            // Global exists but not enabled, still prefer it over local
            (global, false)
        } else {
            (local_browser_manager, false)
        };
        
        // Get current browser URL for display
        let mut browser_url = None;
        
        if browser_enabled {
            tracing::info!("Browser is enabled, attempting to capture screenshot...");
            
            // We need to capture screenshots synchronously before sending the message
            // Use a channel to get the result from the async task
            let (tx, rx) = std::sync::mpsc::channel();
            let browser_manager_capture = browser_manager.clone();
            
            tokio::spawn(async move {
                tracing::info!("Starting async screenshot capture...");
                let result = browser_manager_capture.capture_screenshot_with_url().await;
                match &result {
                    Ok((paths, _)) => tracing::info!("Screenshot capture succeeded with {} images", paths.len()),
                    Err(e) => tracing::error!("Screenshot capture failed: {}", e),
                }
                let _ = tx.send(result);
            });
            
            // Wait for screenshot capture (with timeout)
            match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(Ok((screenshot_paths, url))) => {
                    tracing::info!("Captured {} browser screenshot(s)", screenshot_paths.len());
                    browser_screenshot_count = screenshot_paths.len();
                    let screenshot_url = url.clone();
                    browser_url = url;
                    // Add browser screenshots directly to items (ephemeral - not stored in image_paths for history)
                    for path in screenshot_paths {
                        tracing::info!("Browser screenshot attached: {}", path.display());
                        // Check if the file actually exists
                        if path.exists() {
                            tracing::info!("Screenshot file exists at: {}", path.display());
                            // Add as EphemeralImage with metadata for identification
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            let metadata = format!("screenshot:{}:{}", timestamp, screenshot_url.as_deref().unwrap_or("unknown"));
                            items.push(InputItem::EphemeralImage { 
                                path,
                                metadata: Some(metadata),
                            });
                        } else {
                            tracing::error!("Screenshot file does not exist at: {}", path.display());
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to capture browser screenshot: {}", e);
                }
                Err(_) => {
                    tracing::error!("Browser screenshot capture timed out");
                }
            }
        } else {
            tracing::info!("Browser is not enabled, skipping screenshot capture");
        }

        if !actual_text.is_empty() {
            items.push(InputItem::Text { text: actual_text.clone() });
        }

        // Add user-provided images (these are persistent in history)
        for path in image_paths {
            items.push(InputItem::LocalImage { path });
        }

        if items.is_empty() {
            return;
        }

        // Debug logging for what we're sending
        let ephemeral_count = items.iter().filter(|item| {
            matches!(item, InputItem::EphemeralImage { .. })
        }).count();
        let total_items = items.len();
        if ephemeral_count > 0 {
            tracing::info!("Sending {} items to model (including {} ephemeral images)", total_items, ephemeral_count);
        }

        self.codex_op_tx
            .send(Op::UserInput { items })
            .unwrap_or_else(|e| {
                tracing::error!("failed to send message: {e}");
            });

        // Persist the original text to cross-session message history.
        if !original_text.is_empty() {
            self.codex_op_tx
                .send(Op::AddToHistory { text: original_text.clone() })
                .unwrap_or_else(|e| {
                    tracing::error!("failed to send AddHistory op: {e}");
                });
        }

        // Show text in conversation history, with a note about browser screenshots
        if !original_text.is_empty() {
            let display_text = if browser_screenshot_count > 0 {
                if let Some(url) = browser_url {
                    format!("{}\n\n[Browser: {}]", original_text, url)
                } else {
                    format!("{}\n\n[Browser screenshot attached]", original_text)
                }
            } else {
                original_text.clone()
            };
            self.add_to_history(HistoryCell::new_user_prompt(display_text));
        } else if browser_screenshot_count > 0 {
            let prompt_text = if let Some(url) = browser_url {
                format!("[Browser: {}]", url)
            } else {
                "[Browser screenshot attached]".to_string()
            };
            self.add_to_history(HistoryCell::new_user_prompt(prompt_text));
        }
    }

    pub(crate) fn handle_codex_event(&mut self, event: Event) {
        let Event { id, msg } = event;
        match msg {
            EventMsg::SessionConfigured(event) => {
                self.bottom_pane
                    .set_history_metadata(event.history_log_id, event.history_entry_count);
                // Record session information at the top of the conversation.
                // Only show welcome message on first SessionConfigured event
                let is_first = !self.welcome_shown;
                if is_first {
                    self.welcome_shown = true;
                }
                self.add_to_history(HistoryCell::new_session_info(&self.config, event, is_first));

                if let Some(user_message) = self.initial_user_message.take() {
                    // If the user provided an initial message, add it to the
                    // conversation history.
                    self.submit_user_message(user_message);
                }

                self.request_redraw();
            }
            EventMsg::AgentMessage(AgentMessageEvent { message: _ }) => {
                // Final assistant answer: commit all remaining rows and close with
                // a blank line. Use the final text if provided, otherwise rely on
                // streamed deltas already in the builder.
                self.finalize_stream(StreamKind::Answer);
                self.request_redraw();
            }
            EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { delta }) => {
                self.begin_stream(StreamKind::Answer);
                self.answer_buffer.push_str(&delta);
                self.stream_push_and_maybe_commit(&delta);
                self.request_redraw();
            }
            EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent { delta }) => {
                // Stream CoT into the live pane; keep input visible and commit
                // overflow rows incrementally to scrollback.
                self.begin_stream(StreamKind::Reasoning);
                self.reasoning_buffer.push_str(&delta);
                self.stream_push_and_maybe_commit(&delta);
                self.request_redraw();
            }
            EventMsg::AgentReasoning(AgentReasoningEvent { text: _ }) => {
                // Final reasoning: commit remaining rows and close with a blank.
                self.finalize_stream(StreamKind::Reasoning);
                self.request_redraw();
            }
            EventMsg::AgentReasoningRawContentDelta(AgentReasoningRawContentDeltaEvent {
                delta,
            }) => {
                // Treat raw reasoning content the same as summarized reasoning for UI flow.
                self.begin_stream(StreamKind::Reasoning);
                self.reasoning_buffer.push_str(&delta);
                self.stream_push_and_maybe_commit(&delta);
                self.request_redraw();
            }
            EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { text: _ }) => {
                // Finalize the raw reasoning stream just like the summarized reasoning event.
                self.finalize_stream(StreamKind::Reasoning);
                self.request_redraw();
            }
            EventMsg::TaskStarted => {
                self.bottom_pane.clear_ctrl_c_quit_hint();
                self.bottom_pane.set_task_running(true);
                // Replace composer with single-line spinner while waiting.
                self.bottom_pane
                    .update_status_text("waiting for model".to_string());
                self.request_redraw();
            }
            EventMsg::TaskComplete(TaskCompleteEvent {
                last_agent_message: _,
            }) => {
                self.bottom_pane.set_task_running(false);
                self.bottom_pane.clear_live_ring();
                self.request_redraw();
            }
            EventMsg::TokenCount(token_usage) => {
                self.total_token_usage = add_token_usage(&self.total_token_usage, &token_usage);
                self.last_token_usage = token_usage;
                self.bottom_pane.set_token_usage(
                    self.total_token_usage.clone(),
                    self.last_token_usage.clone(),
                    self.config.model_context_window,
                );
            }
            EventMsg::Error(ErrorEvent { message }) => {
                self.add_to_history(HistoryCell::new_error_event(message.clone()));
                self.bottom_pane.set_task_running(false);
                self.bottom_pane.clear_live_ring();
                self.live_builder = RowBuilder::new(self.live_builder.width());
                self.current_stream = None;
                self.stream_header_emitted = false;
                self.answer_buffer.clear();
                self.reasoning_buffer.clear();
                self.content_buffer.clear();
                self.request_redraw();
            }
            EventMsg::PlanUpdate(update) => {
                // Commit plan updates directly to history (no status-line preview).
                self.add_to_history(HistoryCell::new_plan_update(update));
            }
            EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                call_id: _,
                command,
                cwd,
                reason,
            }) => {
                self.finalize_active_stream();
                let request = ApprovalRequest::Exec {
                    id,
                    command,
                    cwd,
                    reason,
                };
                self.bottom_pane.push_approval_request(request);
                self.request_redraw();
            }
            EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
                call_id: _,
                changes,
                reason,
                grant_root,
            }) => {
                self.finalize_active_stream();
                // ------------------------------------------------------------------
                // Before we even prompt the user for approval we surface the patch
                // summary in the main conversation so that the dialog appears in a
                // sensible chronological order:
                //   (1) codex → proposes patch (HistoryCell::PendingPatch)
                //   (2) UI → asks for approval (BottomPane)
                // This mirrors how command execution is shown (command begins →
                // approval dialog) and avoids surprising the user with a modal
                // prompt before they have seen *what* is being requested.
                // ------------------------------------------------------------------
                self.add_to_history(HistoryCell::new_patch_event(
                    PatchEventType::ApprovalRequest,
                    changes,
                ));

                // Now surface the approval request in the BottomPane as before.
                let request = ApprovalRequest::ApplyPatch {
                    id,
                    reason,
                    grant_root,
                };
                self.bottom_pane.push_approval_request(request);
                self.request_redraw();
            }
            EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                call_id,
                command,
                cwd,
            }) => {
                self.finalize_active_stream();
                // Ensure the status indicator is visible while the command runs.
                self.bottom_pane
                    .update_status_text("running command".to_string());
                self.running_commands.insert(
                    call_id,
                    RunningCommand {
                        command: command.clone(),
                        cwd: cwd.clone(),
                    },
                );
                self.active_history_cell = Some(HistoryCell::new_active_exec_command(command));
            }
            EventMsg::ExecCommandOutputDelta(_) => {
                // TODO
            }
            EventMsg::PatchApplyBegin(PatchApplyBeginEvent {
                call_id: _,
                auto_approved,
                changes,
            }) => {
                self.add_to_history(HistoryCell::new_patch_event(
                    PatchEventType::ApplyBegin { auto_approved },
                    changes,
                ));
            }
            EventMsg::PatchApplyEnd(event) => {
                if !event.success {
                    self.add_to_history(HistoryCell::new_patch_apply_failure(event.stderr));
                }
            }
            EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                call_id,
                exit_code,
                duration: _,
                stdout,
                stderr,
            }) => {
                // Compute summary before moving stdout into the history cell.
                let cmd = self.running_commands.remove(&call_id);
                self.active_history_cell = None;
                self.add_to_history(HistoryCell::new_completed_exec_command(
                    cmd.map(|cmd| cmd.command).unwrap_or_else(|| vec![call_id]),
                    CommandOutput {
                        exit_code,
                        stdout,
                        stderr,
                    },
                ));
            }
            EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
                call_id: _,
                invocation,
            }) => {
                self.finalize_active_stream();
                self.add_to_history(HistoryCell::new_active_mcp_tool_call(invocation));
            }
            EventMsg::McpToolCallEnd(McpToolCallEndEvent {
                call_id: _,
                duration,
                invocation,
                result,
            }) => {
                self.add_to_history(HistoryCell::new_completed_mcp_tool_call(
                    80,
                    invocation,
                    duration,
                    result
                        .as_ref()
                        .map(|r| r.is_error.unwrap_or(false))
                        .unwrap_or(false),
                    result,
                ));
            }
            EventMsg::GetHistoryEntryResponse(event) => {
                let codex_core::protocol::GetHistoryEntryResponseEvent {
                    offset,
                    log_id,
                    entry,
                } = event;

                // Inform bottom pane / composer.
                self.bottom_pane
                    .on_history_entry_response(log_id, offset, entry.map(|e| e.text));
            }
            EventMsg::ShutdownComplete => {
                self.app_event_tx.send(AppEvent::ExitRequest);
            }
            EventMsg::TurnDiff(TurnDiffEvent { unified_diff }) => {
                info!("TurnDiffEvent: {unified_diff}");
            }
            EventMsg::BackgroundEvent(BackgroundEventEvent { message }) => {
                info!("BackgroundEvent: {message}");
            }
        }
    }

    /// Update the live log preview while a task is running.
    pub(crate) fn update_latest_log(&mut self, line: String) {
        if self.bottom_pane.is_task_running() {
            self.bottom_pane.update_status_text(line);
        }
    }

    fn request_redraw(&mut self) {
        self.app_event_tx.send(AppEvent::RequestRedraw);
    }

    pub(crate) fn add_diff_output(&mut self, diff_output: String) {
        self.add_to_history(HistoryCell::new_diff_output(diff_output.clone()));
    }

    pub(crate) fn add_status_output(&mut self) {
        self.add_to_history(HistoryCell::new_status_output(
            &self.config,
            &self.total_token_usage,
        ));
    }

    pub(crate) fn add_prompts_output(&mut self) {
        self.add_to_history(HistoryCell::new_prompts_output());
    }

    pub(crate) fn handle_reasoning_command(&mut self, command_text: String) {
        // Parse the command to extract the parameter
        let parts: Vec<&str> = command_text.trim().split_whitespace().collect();
        
        if parts.len() > 1 {
            // User specified a level: /reasoning high
            let new_effort = match parts[1].to_lowercase().as_str() {
                "low" => ReasoningEffort::Low,
                "medium" | "med" => ReasoningEffort::Medium,
                "high" => ReasoningEffort::High,
                "none" | "off" => ReasoningEffort::None,
                _ => {
                    // Invalid parameter, show error and return
                    let message = format!(
                        "Invalid reasoning level: '{}'. Use: low, medium, high, or none",
                        parts[1]
                    );
                    self.add_to_history(HistoryCell::new_error_event(message));
                    return;
                }
            };
            self.set_reasoning_effort(new_effort);
        } else {
            // No parameter - show interactive selection UI
            self.bottom_pane.show_reasoning_selection(self.config.model_reasoning_effort);
            return;
        }
    }
    
    pub(crate) fn set_reasoning_effort(&mut self, new_effort: ReasoningEffort) {
        
        // Update the config
        self.config.model_reasoning_effort = new_effort;
        
        // Send ConfigureSession op to update the backend
        let op = Op::ConfigureSession {
            provider: self.config.model_provider.clone(),
            model: self.config.model.clone(),
            model_reasoning_effort: new_effort,
            model_reasoning_summary: self.config.model_reasoning_summary,
            user_instructions: self.config.user_instructions.clone(),
            base_instructions: self.config.base_instructions.clone(),
            approval_policy: self.config.approval_policy.clone(),
            sandbox_policy: self.config.sandbox_policy.clone(),
            disable_response_storage: self.config.disable_response_storage,
            notify: self.config.notify.clone(),
            cwd: self.config.cwd.clone(),
            resume_path: None,
        };
        
        self.submit_op(op);
        
        // Add status message to history
        self.add_to_history(HistoryCell::new_reasoning_output(new_effort));
    }

    /// Forward file-search results to the bottom pane.
    pub(crate) fn apply_file_search_result(&mut self, query: String, matches: Vec<FileMatch>) {
        self.bottom_pane.on_file_search_result(query, matches);
    }

    pub(crate) fn on_esc(&mut self) -> bool {
        if self.bottom_pane.is_task_running() {
            self.interrupt_running_task();
            return true;
        }
        false
    }

    /// Handle Ctrl-C key press.
    /// Returns CancellationEvent::Handled if the event was consumed by the UI, or
    /// CancellationEvent::Ignored if the caller should handle it (e.g. exit).
    pub(crate) fn on_ctrl_c(&mut self) -> CancellationEvent {
        match self.bottom_pane.on_ctrl_c() {
            CancellationEvent::Handled => return CancellationEvent::Handled,
            CancellationEvent::Ignored => {}
        }
        if self.bottom_pane.is_task_running() {
            self.interrupt_running_task();
            CancellationEvent::Ignored
        } else if self.bottom_pane.ctrl_c_quit_hint_visible() {
            self.submit_op(Op::Shutdown);
            CancellationEvent::Handled
        } else {
            self.bottom_pane.show_ctrl_c_quit_hint();
            CancellationEvent::Ignored
        }
    }

    pub(crate) fn on_ctrl_z(&mut self) {
        self.interrupt_running_task();
    }

    pub(crate) fn composer_is_empty(&self) -> bool {
        self.bottom_pane.composer_is_empty()
    }

    /// Forward an `Op` directly to codex.
    pub(crate) fn submit_op(&self, op: Op) {
        if let Err(e) = self.codex_op_tx.send(op) {
            tracing::error!("failed to submit op: {e}");
        }
    }

    pub(crate) fn handle_browser_command(&mut self, command_text: String) {
        // Parse the browser subcommand
        let parts: Vec<&str> = command_text.trim().split_whitespace().collect();
        
        let browser_manager = self.browser_manager.clone();
        let response = if parts.len() > 1 {
            let first_arg = parts[1];
            
            // Check if the first argument looks like a URL (has a dot or protocol)
            let is_url = first_arg.contains("://") || first_arg.contains(".");
            
            if is_url {
                // It's a URL - enable browser mode and navigate to it
                let url = parts[1..].join(" ");
                
                // Ensure URL has protocol
                let full_url = if !url.contains("://") {
                    format!("https://{}", url)
                } else {
                    url.clone()
                };
                
                // Enable browser mode
                browser_manager.set_enabled_sync(true);
                
                // Navigate to URL and wait for it to load
                let browser_manager_open = browser_manager.clone();
                let url_for_goto = full_url.clone();
                tokio::spawn(async move {
                    match browser_manager_open.goto(&url_for_goto).await {
                        Ok(result) => {
                            tracing::info!("Browser opened to: {} (title: {:?})", result.url, result.title);
                            // Add a small delay to ensure page is fully rendered
                            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                            tracing::info!("Page should be loaded now");
                        }
                        Err(e) => {
                            tracing::error!("Failed to open browser: {}", e);
                        }
                    }
                });
                
                format!("Browser mode enabled. Opening: {}\nScreenshots will be attached to model requests.", full_url)
            } else {
                // It's a subcommand
                match first_arg {
                    "off" => {
                        // Disable browser mode
                        browser_manager.set_enabled_sync(false);
                        // Close any open browser
                        let browser_manager_close = browser_manager.clone();
                        tokio::spawn(async move {
                            if let Err(e) = browser_manager_close.close().await {
                                tracing::error!("Failed to close browser: {}", e);
                            }
                        });
                        "Browser mode disabled.".to_string()
                    }
                    "status" => {
                        // Get status from BrowserManager
                        browser_manager.get_status_sync()
                    }
                "fullpage" => {
                    if parts.len() > 2 {
                        match parts[2] {
                            "on" => {
                                // Enable full-page mode
                                browser_manager.set_fullpage_sync(true);
                                "Full-page screenshot mode enabled (max 8 segments).".to_string()
                            }
                            "off" => {
                                // Disable full-page mode
                                browser_manager.set_fullpage_sync(false);
                                "Full-page screenshot mode disabled.".to_string()
                            }
                            _ => "Usage: /browser fullpage [on|off]".to_string()
                        }
                    } else {
                        "Usage: /browser fullpage [on|off]".to_string()
                    }
                }
                "config" => {
                    if parts.len() > 3 {
                        let key = parts[2];
                        let value = parts[3..].join(" ");
                        // Update browser config
                        match key {
                            "viewport" => {
                                // Parse viewport dimensions like "1920x1080"
                                if let Some((width_str, height_str)) = value.split_once('x') {
                                    if let (Ok(width), Ok(height)) = (width_str.parse::<u32>(), height_str.parse::<u32>()) {
                                        browser_manager.set_viewport_sync(width, height);
                                        format!("Browser viewport updated: {}x{}", width, height)
                                    } else {
                                        "Invalid viewport format. Use: /browser config viewport 1920x1080".to_string()
                                    }
                                } else {
                                    "Invalid viewport format. Use: /browser config viewport 1920x1080".to_string()
                                }
                            }
                            "segments_max" => {
                                if let Ok(max) = value.parse::<usize>() {
                                    browser_manager.set_segments_max_sync(max);
                                    format!("Browser segments_max updated: {}", max)
                                } else {
                                    "Invalid segments_max value. Use a number.".to_string()
                                }
                            }
                            _ => format!("Unknown config key: {}. Available: viewport, segments_max", key)
                        }
                    } else {
                        "Usage: /browser config <key> <value>\nAvailable keys: viewport, segments_max".to_string()
                    }
                }
                    _ => {
                        format!("Unknown browser command: '{}'\nUsage: /browser <url> | off | status | fullpage | config", first_arg)
                    }
                }
            }
        } else {
            "Browser commands:\n• /browser <url> - Open URL and enable browser mode\n• /browser off - Disable browser mode\n• /browser status - Show current status\n• /browser fullpage [on|off] - Toggle full-page mode\n• /browser config <key> <value> - Update configuration".to_string()
        };
        
        // Add the response to the UI as a background event
        let lines = response.lines().map(|line| Line::from(line.to_string())).collect();
        self.add_to_history(HistoryCell::BackgroundEvent {
            view: TextBlock::new(lines),
        });
    }

    /// Programmatically submit a user text message as if typed in the
    /// composer. The text will be added to conversation history and sent to
    /// the agent. This also handles slash command expansion.
    pub(crate) fn submit_text_message(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.submit_user_message(text.into());
    }

    pub(crate) fn token_usage(&self) -> &TokenUsage {
        &self.total_token_usage
    }

    pub(crate) fn clear_token_usage(&mut self) {
        self.total_token_usage = TokenUsage::default();
        self.bottom_pane.set_token_usage(
            self.total_token_usage.clone(),
            self.last_token_usage.clone(),
            self.config.model_context_window,
        );
    }

    pub fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let [_, bottom_pane_area] = self.layout_areas(area);
        self.bottom_pane.cursor_pos(bottom_pane_area)
    }
}

impl ChatWidget<'_> {
    fn begin_stream(&mut self, kind: StreamKind) {
        if let Some(current) = self.current_stream {
            if current != kind {
                self.finalize_stream(current);
            }
        }

        if self.current_stream != Some(kind) {
            self.current_stream = Some(kind);
            self.stream_header_emitted = false;
            // Clear any previous live content; we're starting a new stream.
            self.live_builder = RowBuilder::new(self.live_builder.width());
            // Ensure the waiting status is visible (composer replaced).
            self.bottom_pane
                .update_status_text("waiting for model".to_string());
            self.emit_stream_header(kind);
        }
    }

    fn stream_push_and_maybe_commit(&mut self, delta: &str) {
        self.live_builder.push_fragment(delta);

        // Commit overflow rows (small batches) while keeping the last N rows visible.
        let drained = self
            .live_builder
            .drain_commit_ready(self.live_max_rows as usize);
        if !drained.is_empty() {
            let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
            if !self.stream_header_emitted {
                match self.current_stream {
                    Some(StreamKind::Reasoning) => {
                        lines.push(ratatui::text::Line::from("thinking".magenta().italic()));
                    }
                    Some(StreamKind::Answer) => {
                        lines.push(ratatui::text::Line::from("codex".magenta().bold()));
                    }
                    None => {}
                }
                self.stream_header_emitted = true;
            }
            for r in drained {
                lines.push(ratatui::text::Line::from(r.text));
            }
            self.app_event_tx.send(AppEvent::InsertHistory(lines));
        }

        // Update the live ring overlay lines (text-only, newest at bottom).
        let rows = self
            .live_builder
            .display_rows()
            .into_iter()
            .map(|r| ratatui::text::Line::from(r.text))
            .collect::<Vec<_>>();
        self.bottom_pane
            .set_live_ring_rows(self.live_max_rows, rows);
    }

    fn finalize_stream(&mut self, kind: StreamKind) {
        if self.current_stream != Some(kind) {
            // Nothing to do; either already finalized or not the active stream.
            return;
        }
        // Flush any partial line as a full row, then drain all remaining rows.
        self.live_builder.end_line();
        let remaining = self.live_builder.drain_rows();
        // TODO: Re-add markdown rendering for assistant answers and reasoning.
        // When finalizing, pass the accumulated text through `markdown::append_markdown`
        // to build styled `Line<'static>` entries instead of raw plain text lines.
        if !remaining.is_empty() || !self.stream_header_emitted {
            let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
            if !self.stream_header_emitted {
                match kind {
                    StreamKind::Reasoning => {
                        lines.push(ratatui::text::Line::from("thinking".magenta().italic()));
                    }
                    StreamKind::Answer => {
                        lines.push(ratatui::text::Line::from("codex".magenta().bold()));
                    }
                }
                self.stream_header_emitted = true;
            }
            for r in remaining {
                lines.push(ratatui::text::Line::from(r.text));
            }
            // Close the block with a blank line for readability.
            lines.push(ratatui::text::Line::from(""));
            self.app_event_tx.send(AppEvent::InsertHistory(lines));
        }

        // Clear the live overlay and reset state for the next stream.
        self.live_builder = RowBuilder::new(self.live_builder.width());
        self.bottom_pane.clear_live_ring();
        self.current_stream = None;
        self.stream_header_emitted = false;
    }
}

impl WidgetRef for &ChatWidget<'_> {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let [active_cell_area, bottom_pane_area] = self.layout_areas(area);
        (&self.bottom_pane).render(bottom_pane_area, buf);
        if let Some(cell) = &self.active_history_cell {
            cell.render_ref(active_cell_area, buf);
        }
    }
}

fn add_token_usage(current_usage: &TokenUsage, new_usage: &TokenUsage) -> TokenUsage {
    let cached_input_tokens = match (
        current_usage.cached_input_tokens,
        new_usage.cached_input_tokens,
    ) {
        (Some(current), Some(new)) => Some(current + new),
        (Some(current), None) => Some(current),
        (None, Some(new)) => Some(new),
        (None, None) => None,
    };
    let reasoning_output_tokens = match (
        current_usage.reasoning_output_tokens,
        new_usage.reasoning_output_tokens,
    ) {
        (Some(current), Some(new)) => Some(current + new),
        (Some(current), None) => Some(current),
        (None, Some(new)) => Some(new),
        (None, None) => None,
    };
    TokenUsage {
        input_tokens: current_usage.input_tokens + new_usage.input_tokens,
        cached_input_tokens,
        output_tokens: current_usage.output_tokens + new_usage.output_tokens,
        reasoning_output_tokens,
        total_tokens: current_usage.total_tokens + new_usage.total_tokens,
    }
}
