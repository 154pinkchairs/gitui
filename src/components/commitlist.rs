use super::utils::logitems::{ItemBatch, LogEntry};
use crate::{
	components::{
		utils::string_width_align, CommandBlocking, CommandInfo,
		Component, DrawableComponent, EventState, ScrollType,
	},
	keys::{key_match, SharedKeyConfig},
	queue::{InternalEvent, Queue},
	strings::{self, symbol},
	try_or_popup,
	ui::style::{SharedTheme, Theme},
	ui::{calc_scroll_top, draw_scrollbar, Orientation},
};
use anyhow::Result;
use asyncgit::sync::{
	self, checkout_commit, BranchDetails, BranchInfo, CommitId,
	RepoPathRef, Tags,
};
use chrono::{DateTime, Local};
use crossterm::event::Event;
use indexmap::IndexSet;
use itertools::Itertools;
use ratatui::{
	backend::Backend,
	layout::{Alignment, Rect},
	style::Style,
	text::{Line, Span},
	widgets::{Block, Borders, Paragraph},
	Frame,
};
use std::{
	borrow::Cow, cell::Cell, cmp, collections::BTreeMap,
	convert::TryFrom, rc::Rc, time::Instant,
};

const ELEMENTS_PER_LINE: usize = 9;
const SLICE_SIZE: usize = 1200;

///
pub struct CommitList {
	repo: RepoPathRef,
	title: Box<str>,
	selection: usize,
	highlighted_selection: Option<usize>,
	items: ItemBatch,
	highlights: Option<Rc<IndexSet<CommitId>>>,
	commits: Vec<CommitId>,
	marked: Vec<(usize, CommitId)>,
	scroll_state: (Instant, f32),
	tags: Option<Tags>,
	local_branches: BTreeMap<CommitId, Vec<BranchInfo>>,
	remote_branches: BTreeMap<CommitId, Vec<BranchInfo>>,
	current_size: Cell<Option<(u16, u16)>>,
	scroll_top: Cell<usize>,
	theme: SharedTheme,
	queue: Queue,
	key_config: SharedKeyConfig,
}

impl CommitList {
	///
	pub fn new(
		repo: RepoPathRef,
		title: &str,
		theme: SharedTheme,
		queue: Queue,
		key_config: SharedKeyConfig,
	) -> Self {
		Self {
			repo,
			items: ItemBatch::default(),
			marked: Vec::with_capacity(2),
			selection: 0,
			highlighted_selection: None,
			commits: Vec::new(),
			highlights: None,
			scroll_state: (Instant::now(), 0_f32),
			tags: None,
			local_branches: BTreeMap::default(),
			remote_branches: BTreeMap::default(),
			current_size: Cell::new(None),
			scroll_top: Cell::new(0),
			theme,
			queue,
			key_config,
			title: title.into(),
		}
	}

	///
	pub const fn tags(&self) -> Option<&Tags> {
		self.tags.as_ref()
	}

	///
	pub fn clear(&mut self) {
		self.items.clear();
		self.commits.clear();
	}

	///
	pub fn copy_items(&self) -> Vec<CommitId> {
		self.commits.clone()
	}

	///
	pub fn set_tags(&mut self, tags: Tags) {
		self.tags = Some(tags);
	}

	///
	pub fn selected_entry(&self) -> Option<&LogEntry> {
		self.items.iter().nth(
			self.selection.saturating_sub(self.items.index_offset()),
		)
	}

	///
	pub fn marked_count(&self) -> usize {
		self.marked.len()
	}

	///
	pub fn marked(&self) -> &[(usize, CommitId)] {
		&self.marked
	}

	///
	pub fn clear_marked(&mut self) {
		self.marked.clear();
	}

	///
	pub fn marked_commits(&self) -> Vec<CommitId> {
		let (_, commits): (Vec<_>, Vec<CommitId>) =
			self.marked.iter().copied().unzip();

		commits
	}

	///
	pub fn copy_commit_hash(&self) -> Result<()> {
		let marked = self.marked.as_slice();
		let yank: Option<String> = match marked {
			[] => self
				.items
				.iter()
				.nth(
					self.selection
						.saturating_sub(self.items.index_offset()),
				)
				.map(|e| e.id.to_string()),
			[(_idx, commit)] => Some(commit.to_string()),
			[first, .., last] => {
				let marked_consecutive =
					marked.windows(2).all(|w| w[0].0 + 1 == w[1].0);

				let yank = if marked_consecutive {
					format!(
						"{}^..{}",
						first.1.to_string(),
						last.1.to_string()
					)
				} else {
					marked
						.iter()
						.map(|(_idx, commit)| commit.to_string())
						.join(" ")
				};
				Some(yank)
			}
		};

		if let Some(yank) = yank {
			crate::clipboard::copy_string(&yank)?;
			self.queue.push(InternalEvent::ShowInfoMsg(
				strings::copy_success(&yank),
			));
		}
		Ok(())
	}

	///
	pub fn checkout(&mut self) {
		if let Some(commit_hash) =
			self.selected_entry().map(|entry| entry.id)
		{
			try_or_popup!(
				self,
				"failed to checkout commit:",
				checkout_commit(&self.repo.borrow(), commit_hash)
			);
		}
	}

	///
	pub fn set_local_branches(
		&mut self,
		local_branches: Vec<BranchInfo>,
	) {
		self.local_branches.clear();

		for local_branch in local_branches {
			self.local_branches
				.entry(local_branch.top_commit)
				.or_default()
				.push(local_branch);
		}
	}

	///
	pub fn set_remote_branches(
		&mut self,
		remote_branches: Vec<BranchInfo>,
	) {
		self.remote_branches.clear();

		for remote_branch in remote_branches {
			self.remote_branches
				.entry(remote_branch.top_commit)
				.or_default()
				.push(remote_branch);
		}
	}

	///
	pub fn set_commits(&mut self, commits: Vec<CommitId>) {
		self.commits = commits;
		self.fetch_commits(false);
	}

	///
	pub fn refresh_extend_data(&mut self, commits: Vec<CommitId>) {
		let new_commits = !commits.is_empty();
		self.commits.extend(commits);

		let selection = self.selection();
		let selection_max = self.selection_max();

		if self.needs_data(selection, selection_max) || new_commits {
			self.fetch_commits(false);
		}
	}

	///
	pub fn set_highlighting(
		&mut self,
		highlighting: Option<Rc<IndexSet<CommitId>>>,
	) {
		//note: set highlights to none if there is no highlight
		self.highlights = if highlighting
			.as_ref()
			.map(|set| set.is_empty())
			.unwrap_or_default()
		{
			None
		} else {
			highlighting
		};

		self.select_next_highlight();
		self.set_highlighted_selection_index();
		self.fetch_commits(true);
	}

	///
	pub fn select_commit(&mut self, id: CommitId) -> Result<()> {
		let position = self.commits.iter().position(|&x| x == id);

		if let Some(position) = position {
			self.selection = position;
			self.set_highlighted_selection_index();
			Ok(())
		} else {
			anyhow::bail!("Could not select commit. It might not be loaded yet or it might be on a different branch.");
		}
	}

	///
	pub fn highlighted_selection_info(&self) -> (usize, usize) {
		let amount = self
			.highlights
			.as_ref()
			.map(|highlights| highlights.len())
			.unwrap_or_default();
		(self.highlighted_selection.unwrap_or_default(), amount)
	}

	fn set_highlighted_selection_index(&mut self) {
		self.highlighted_selection =
			self.highlights.as_ref().and_then(|highlights| {
				highlights.iter().position(|entry| {
					entry == &self.commits[self.selection]
				})
			});
	}

	const fn selection(&self) -> usize {
		self.selection
	}

	/// will return view size or None before the first render
	fn current_size(&self) -> Option<(u16, u16)> {
		self.current_size.get()
	}

	#[allow(clippy::missing_const_for_fn)]
	fn selection_max(&self) -> usize {
		self.commits.len().saturating_sub(1)
	}

	fn selected_entry_marked(&self) -> bool {
		self.selected_entry()
			.and_then(|e| self.is_marked(&e.id))
			.unwrap_or_default()
	}

	fn move_selection(&mut self, scroll: ScrollType) -> Result<bool> {
		let needs_update = if self.items.highlighting() {
			self.move_selection_highlighting(scroll)?
		} else {
			self.move_selection_normal(scroll)?
		};

		Ok(needs_update)
	}

	fn move_selection_highlighting(
		&mut self,
		scroll: ScrollType,
	) -> Result<bool> {
		let old_selection = self.selection;

		loop {
			let new_selection = match scroll {
				ScrollType::Up => self.selection.saturating_sub(1),
				ScrollType::Down => self.selection.saturating_add(1),

				//TODO: support this?
				// ScrollType::Home => 0,
				// ScrollType::End => self.selection_max(),
				_ => return Ok(false),
			};

			let new_selection =
				cmp::min(new_selection, self.selection_max());
			let selection_changed = new_selection != self.selection;

			if !selection_changed {
				self.selection = old_selection;
				return Ok(false);
			}

			self.selection = new_selection;

			if self.selection_highlighted() {
				self.set_highlighted_selection_index();
				return Ok(true);
			}
		}
	}

	fn move_selection_normal(
		&mut self,
		scroll: ScrollType,
	) -> Result<bool> {
		self.update_scroll_speed();

		#[allow(clippy::cast_possible_truncation)]
		let speed_int = usize::try_from(self.scroll_state.1 as i64)?.max(1);

		let page_offset = usize::from(
			self.current_size.get().unwrap_or_default().1,
		)
		.saturating_sub(1);

		let new_selection = match scroll {
			ScrollType::Up => {
				self.selection.saturating_sub(speed_int)
			}
			ScrollType::Down => {
				self.selection.saturating_add(speed_int)
			}
			ScrollType::PageUp => {
				self.selection.saturating_sub(page_offset)
			}
			ScrollType::PageDown => {
				self.selection.saturating_add(page_offset)
			}
			ScrollType::Home => 0,
			ScrollType::End => self.selection_max(),
		};

		let new_selection =
			cmp::min(new_selection, self.selection_max());
		let needs_update = new_selection != self.selection;

		self.selection = new_selection;

		Ok(needs_update)
	}

	fn mark(&mut self) {
		if let Some(e) = self.selected_entry() {
			let id = e.id;
			let selected = self
				.selection
				.saturating_sub(self.items.index_offset());
			if self.is_marked(&id).unwrap_or_default() {
				self.marked.retain(|marked| marked.1 != id);
			} else {
				self.marked.push((selected, id));

				self.marked.sort_unstable_by(|first, second| {
					first.0.cmp(&second.0)
				});
			}
		}
	}

	fn update_scroll_speed(&mut self) {
		const REPEATED_SCROLL_THRESHOLD_MILLIS: u128 = 300;
		const SCROLL_SPEED_START: f32 = 0.1_f32;
		const SCROLL_SPEED_MAX: f32 = 10_f32;
		const SCROLL_SPEED_MULTIPLIER: f32 = 1.05_f32;

		let now = Instant::now();

		let since_last_scroll =
			now.duration_since(self.scroll_state.0);

		self.scroll_state.0 = now;

		let speed = if since_last_scroll.as_millis()
			< REPEATED_SCROLL_THRESHOLD_MILLIS
		{
			self.scroll_state.1 * SCROLL_SPEED_MULTIPLIER
		} else {
			SCROLL_SPEED_START
		};

		self.scroll_state.1 = speed.min(SCROLL_SPEED_MAX);
	}

	fn is_marked(&self, id: &CommitId) -> Option<bool> {
		if self.marked.is_empty() {
			None
		} else {
			let found =
				self.marked.iter().any(|entry| entry.1 == *id);
			Some(found)
		}
	}

	#[allow(clippy::too_many_arguments)]
	fn get_entry_to_add<'a>(
		&self,
		e: &'a LogEntry,
		selected: bool,
		tags: Option<String>,
		local_branches: Option<String>,
		remote_branches: Option<String>,
		theme: &Theme,
		width: usize,
		now: DateTime<Local>,
		marked: Option<bool>,
	) -> Line<'a> {
		let mut txt: Vec<Span> = Vec::with_capacity(
			ELEMENTS_PER_LINE + if marked.is_some() { 2 } else { 0 },
		);

		let normal = !self.items.highlighting()
			|| (self.items.highlighting() && e.highlighted);

		let splitter_txt = Cow::from(symbol::EMPTY_SPACE);
		let splitter = Span::styled(
			splitter_txt,
			if normal {
				theme.text(true, selected)
			} else {
				Style::default()
			},
		);

		// marker
		if let Some(marked) = marked {
			txt.push(Span::styled(
				Cow::from(if marked {
					symbol::CHECKMARK
				} else {
					symbol::EMPTY_SPACE
				}),
				theme.log_marker(selected),
			));
			txt.push(splitter.clone());
		}

		let style_hash = normal
			.then(|| theme.commit_hash(selected))
			.unwrap_or_else(|| theme.commit_unhighlighted());
		let style_time = normal
			.then(|| theme.commit_time(selected))
			.unwrap_or_else(|| theme.commit_unhighlighted());
		let style_author = normal
			.then(|| theme.commit_author(selected))
			.unwrap_or_else(|| theme.commit_unhighlighted());
		let style_tags = normal
			.then(|| theme.tags(selected))
			.unwrap_or_else(|| theme.commit_unhighlighted());
		let style_branches = normal
			.then(|| theme.branch(selected, true))
			.unwrap_or_else(|| theme.commit_unhighlighted());
		let style_msg = normal
			.then(|| theme.text(true, selected))
			.unwrap_or_else(|| theme.commit_unhighlighted());

		// commit hash
		txt.push(Span::styled(Cow::from(&*e.hash_short), style_hash));

		txt.push(splitter.clone());

		// commit timestamp
		txt.push(Span::styled(
			Cow::from(e.time_to_string(now)),
			style_time,
		));

		txt.push(splitter.clone());

		let author_width =
			(width.saturating_sub(19) / 3).clamp(3, 20);
		let author = string_width_align(&e.author, author_width);

		// commit author
		txt.push(Span::styled::<String>(author, style_author));

		txt.push(splitter.clone());

		// commit tags
		if let Some(tags) = tags {
			txt.push(splitter.clone());
			txt.push(Span::styled(tags, style_tags));
		}

		if let Some(local_branches) = local_branches {
			txt.push(splitter.clone());
			txt.push(Span::styled(local_branches, style_branches));
		}
		if let Some(remote_branches) = remote_branches {
			txt.push(splitter.clone());
			txt.push(Span::styled(remote_branches, style_branches));
		}

		txt.push(splitter);

		let message_width = width.saturating_sub(
			txt.iter().map(|span| span.content.len()).sum(),
		);

		// commit msg
		txt.push(Span::styled(
			format!("{:message_width$}", &e.msg),
			style_msg,
		));

		Line::from(txt)
	}

	fn get_text(&self, height: usize, width: usize) -> Vec<Line> {
		let selection = self.relative_selection();

		let mut txt: Vec<Line> = Vec::with_capacity(height);

		let now = Local::now();

		let any_marked = !self.marked.is_empty();

		for (idx, e) in self
			.items
			.iter()
			.skip(self.scroll_top.get())
			.take(height)
			.enumerate()
		{
			let tags =
				self.tags.as_ref().and_then(|t| t.get(&e.id)).map(
					|tags| {
						tags.iter()
							.map(|t| format!("<{}>", t.name))
							.join(" ")
					},
				);

			let local_branches =
				self.local_branches.get(&e.id).map(|local_branch| {
					local_branch
						.iter()
						.map(|local_branch| {
							format!("{{{0}}}", local_branch.name)
						})
						.join(" ")
				});

			let marked = if any_marked {
				self.is_marked(&e.id)
			} else {
				None
			};

			txt.push(self.get_entry_to_add(
				e,
				idx + self.scroll_top.get() == selection,
				tags,
				local_branches,
				self.remote_branches_string(e),
				&self.theme,
				width,
				now,
				marked,
			));
		}

		txt
	}

	fn remote_branches_string(&self, e: &LogEntry) -> Option<String> {
		self.remote_branches.get(&e.id).and_then(|remote_branches| {
			let filtered_branches: Vec<_> = remote_branches
				.iter()
				.filter(|remote_branch| {
					self.local_branches.get(&e.id).map_or(
						true,
						|local_branch| {
							local_branch.iter().any(|local_branch| {
								let has_corresponding_local_branch =
									match &local_branch.details {
										BranchDetails::Local(
											details,
										) => details
											.upstream
											.as_ref()
											.map_or(
												false,
												|upstream| {
													upstream.reference == remote_branch.reference
												},
											),
										BranchDetails::Remote(_) => {
											false
										}
									};

								!has_corresponding_local_branch
							})
						},
					)
				})
				.map(|remote_branch| {
					format!("[{0}]", remote_branch.name)
				})
				.collect();

			if filtered_branches.is_empty() {
				None
			} else {
				Some(filtered_branches.join(" "))
			}
		})
	}

	#[allow(clippy::missing_const_for_fn)]
	fn relative_selection(&self) -> usize {
		self.selection.saturating_sub(self.items.index_offset())
	}

	fn select_next_highlight(&mut self) {
		if self.highlights.is_none() {
			return;
		}

		let old_selection = self.selection;

		let mut offset = 0;
		loop {
			let hit_upper_bound =
				old_selection + offset > self.selection_max();
			let hit_lower_bound = offset > old_selection;

			if !hit_upper_bound {
				self.selection = old_selection + offset;

				if self.selection_highlighted() {
					break;
				}
			}

			if !hit_lower_bound {
				self.selection = old_selection - offset;

				if self.selection_highlighted() {
					break;
				}
			}

			if hit_lower_bound && hit_upper_bound {
				self.selection = old_selection;
				break;
			}

			offset += 1;
		}
	}

	fn selection_highlighted(&mut self) -> bool {
		let commit = self.commits[self.selection];

		self.highlights
			.as_ref()
			.map(|highlights| highlights.contains(&commit))
			.unwrap_or_default()
	}

	fn needs_data(&self, idx: usize, idx_max: usize) -> bool {
		self.items.needs_data(idx, idx_max)
	}

	fn fetch_commits(&mut self, force: bool) {
		let want_min =
			self.selection().saturating_sub(SLICE_SIZE / 2);
		let commits = self.commits.len();

		let want_min = want_min.min(commits);

		if !self
			.items
			.index_offset_raw()
			.map(|index| want_min == index)
			.unwrap_or_default()
			|| force
		{
			let slice_end =
				want_min.saturating_add(SLICE_SIZE).min(commits);

			let commits = sync::get_commits_info(
				&self.repo.borrow(),
				&self.commits[want_min..slice_end],
				self.current_size()
					.map_or(100u16, |size| size.0)
					.into(),
			);

			if let Ok(commits) = commits {
				self.items.set_items(
					want_min,
					commits,
					&self.highlights,
				);
			}
		}
	}
}

impl DrawableComponent for CommitList {
	fn draw<B: Backend>(
		&self,
		f: &mut Frame<B>,
		area: Rect,
	) -> Result<()> {
		let current_size = (
			area.width.saturating_sub(2),
			area.height.saturating_sub(2),
		);
		self.current_size.set(Some(current_size));

		let height_in_lines = current_size.1 as usize;
		let selection = self.relative_selection();

		self.scroll_top.set(calc_scroll_top(
			self.scroll_top.get(),
			height_in_lines,
			selection,
		));

		let title = format!(
			"{} {}/{}",
			self.title,
			self.commits.len().saturating_sub(self.selection),
			self.commits.len(),
		);

		f.render_widget(
			Paragraph::new(
				self.get_text(
					height_in_lines,
					current_size.0 as usize,
				),
			)
			.block(
				Block::default()
					.borders(Borders::ALL)
					.title(Span::styled(
						title.as_str(),
						self.theme.title(true),
					))
					.border_style(self.theme.block(true)),
			)
			.alignment(Alignment::Left),
			area,
		);

		draw_scrollbar(
			f,
			area,
			&self.theme,
			self.commits.len(),
			self.selection,
			Orientation::Vertical,
		);

		Ok(())
	}
}

impl Component for CommitList {
	fn event(&mut self, ev: &Event) -> Result<EventState> {
		if let Event::Key(k) = ev {
			let selection_changed =
				if key_match(k, self.key_config.keys.move_up) {
					self.move_selection(ScrollType::Up)?
				} else if key_match(k, self.key_config.keys.move_down)
				{
					self.move_selection(ScrollType::Down)?
				} else if key_match(k, self.key_config.keys.shift_up)
					|| key_match(k, self.key_config.keys.home)
				{
					self.move_selection(ScrollType::Home)?
				} else if key_match(
					k,
					self.key_config.keys.shift_down,
				) || key_match(k, self.key_config.keys.end)
				{
					self.move_selection(ScrollType::End)?
				} else if key_match(k, self.key_config.keys.page_up) {
					self.move_selection(ScrollType::PageUp)?
				} else if key_match(k, self.key_config.keys.page_down)
				{
					self.move_selection(ScrollType::PageDown)?
				} else if key_match(
					k,
					self.key_config.keys.log_mark_commit,
				) {
					self.mark();
					true
				} else if key_match(
					k,
					self.key_config.keys.log_checkout_commit,
				) {
					self.checkout();
					true
				} else {
					false
				};
			return Ok(selection_changed.into());
		}

		Ok(EventState::NotConsumed)
	}

	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		_force_all: bool,
	) -> CommandBlocking {
		out.push(CommandInfo::new(
			strings::commands::scroll(&self.key_config),
			self.selected_entry().is_some(),
			true,
		));
		out.push(CommandInfo::new(
			strings::commands::commit_list_mark(
				&self.key_config,
				self.selected_entry_marked(),
			),
			true,
			true,
		));
		CommandBlocking::PassingOn
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_string_width_align() {
		assert_eq!(string_width_align("123", 3), "123");
		assert_eq!(string_width_align("123", 2), "..");
		assert_eq!(string_width_align("123", 3), "123");
		assert_eq!(string_width_align("12345", 6), "12345 ");
		assert_eq!(string_width_align("1234556", 4), "12..");
	}

	#[test]
	fn test_string_width_align_unicode() {
		assert_eq!(string_width_align("äste", 3), "ä..");
		assert_eq!(
			string_width_align("wüsten äste", 10),
			"wüsten ä.."
		);
		assert_eq!(
			string_width_align("Jon Grythe Stødle", 19),
			"Jon Grythe Stødle  "
		);
	}
}
