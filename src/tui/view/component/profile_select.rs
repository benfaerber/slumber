//! Components related to the selection of profiles

use crate::{
    collection::{Profile, ProfileId},
    tui::{
        context::TuiContext,
        input::Action,
        message::MessageSender,
        view::{
            common::{
                list::List, modal::Modal, table::Table,
                template_preview::TemplatePreview, Pane,
            },
            draw::{Draw, Generate},
            event::{Event, EventHandler, EventQueue, Update},
            state::{
                persistence::{
                    Persistable, Persistent, PersistentKey, PersistentOption,
                },
                select::SelectState,
                StateCell,
            },
            Component, ModalPriority,
        },
    },
    util::doc_link,
};
use itertools::Itertools;
use ratatui::{
    layout::{Constraint, Layout},
    prelude::Rect,
    text::Text,
    Frame,
};

/// Minimal pane to show the current profile, and handle interaction to open the
/// profile list modal
#[derive(Debug)]
pub struct ProfilePane {
    /// Store the full list of profiles so we can build a select state when
    /// opening the modal. Clone clone clone!!
    profiles: Vec<Profile>,
    /// ID of the currently selected profile. `PersistentOption` wrapper gets
    /// around the orphan rule.
    selected_profile: Persistent<PersistentOption<ProfileId>>,
}

impl ProfilePane {
    pub fn new(profiles: Vec<Profile>) -> Self {
        Self {
            profiles,
            selected_profile: Persistent::new(
                PersistentKey::ProfileId,
                Default::default(),
            ),
        }
    }

    pub fn selected_profile(&self) -> Option<&Profile> {
        self.selected_profile.as_ref().and_then(|profile_id| {
            self.profiles
                .iter()
                .find(|profile| &profile.id == profile_id)
        })
    }
}

impl EventHandler for ProfilePane {
    fn update(&mut self, messages_tx: &MessageSender, event: Event) -> Update {
        if let Some(Action::LeftClick | Action::SelectProfileList) =
            event.action()
        {
            EventQueue::open_modal(
                ProfileListModal::new(
                    messages_tx.clone(),
                    self.profiles.clone(),
                    self.selected_profile.as_ref(),
                ),
                ModalPriority::Low,
            );
        } else if let Some(SelectProfile(profile_id)) = event.other() {
            // Handle message from the modal
            *self.selected_profile = Some(profile_id.clone()).into();
            EventQueue::push(Event::HttpLoadRequest);
        } else {
            return Update::Propagate(event);
        }
        Update::Consumed
    }
}

impl Draw for ProfilePane {
    fn draw(&self, frame: &mut Frame, _: (), area: Rect) {
        let title = TuiContext::get()
            .input_engine
            .add_hint("Profile", Action::SelectProfileList);
        let block = Pane {
            title: &title,
            is_focused: false,
        }
        .generate();
        frame.render_widget(&block, area);
        let area = block.inner(area);

        frame.render_widget(
            if let Some(profile) = self.selected_profile() {
                profile.name()
            } else {
                "No profiles defined"
            },
            area,
        );
    }
}

/// Local event to pass selected profile ID from modal back to the parent
struct SelectProfile(ProfileId);

/// Modal to allow user to select a profile from a list and preview profile
/// fields
#[derive(Debug)]
pub struct ProfileListModal {
    select: Component<SelectState<Profile>>,
    detail: Component<ProfileDetail>,
}

impl ProfileListModal {
    pub fn new(
        messages_tx: MessageSender,
        profiles: Vec<Profile>,
        selected_profile: Option<&ProfileId>,
    ) -> Self {
        // Loaded request depends on the profile, so refresh on change
        fn on_submit(profile: &mut Profile) {
            // Close the modal *first*, so the parent can handle the
            // callback event. Jank but it works
            EventQueue::push(Event::CloseModal);
            EventQueue::push(Event::new_other(SelectProfile(
                profile.id.clone(),
            )));
        }

        let select = SelectState::builder(profiles)
            .preselect_opt(selected_profile)
            .on_submit(on_submit)
            .build();
        Self {
            select: select.into(),
            detail: ProfileDetail::new(messages_tx).into(),
        }
    }
}

impl Modal for ProfileListModal {
    fn title(&self) -> &str {
        "Profiles"
    }

    fn dimensions(&self) -> (Constraint, Constraint) {
        (Constraint::Percentage(60), Constraint::Percentage(40))
    }
}

impl EventHandler for ProfileListModal {
    fn children(&mut self) -> Vec<Component<&mut dyn EventHandler>> {
        vec![self.select.as_child()]
    }
}

impl Draw for ProfileListModal {
    fn draw(&self, frame: &mut Frame, _: (), area: Rect) {
        // Empty state
        let items = self.select.data().items();
        if items.is_empty() {
            frame.render_widget(
                Text::from(vec![
                    "No profiles defined; add one to your collection.".into(),
                    doc_link("api/request_collection/profile").into(),
                ]),
                area,
            );
            return;
        }

        let [list_area, _, detail_area] = Layout::vertical([
            Constraint::Length(items.len().min(5) as u16),
            Constraint::Length(1), // Padding
            Constraint::Min(0),
        ])
        .areas(area);

        self.select.draw(
            frame,
            List {
                pane: None,
                list: items,
            }
            .generate(),
            list_area,
        );
        if let Some(profile) = self.select.data().selected() {
            self.detail
                .draw(frame, ProfileDetailProps { profile }, detail_area)
        }
    }
}

/// Display the contents of a profile
#[derive(derive_more::Debug)]
pub struct ProfileDetail {
    /// Needed for template preview rendering
    messages_tx: MessageSender,
    #[debug(skip)]
    fields: StateCell<ProfileId, Vec<(String, TemplatePreview)>>,
}

pub struct ProfileDetailProps<'a> {
    pub profile: &'a Profile,
}

impl ProfileDetail {
    pub fn new(messages_tx: MessageSender) -> Self {
        Self {
            messages_tx,
            fields: Default::default(),
        }
    }
}

impl<'a> Draw<ProfileDetailProps<'a>> for ProfileDetail {
    fn draw(
        &self,
        frame: &mut Frame,
        props: ProfileDetailProps<'a>,
        area: Rect,
    ) {
        // Whenever the selected profile changes, rebuild the internal state.
        // This is needed because the template preview rendering is async.
        let fields =
            self.fields.get_or_update(props.profile.id.clone(), || {
                props
                    .profile
                    .data
                    .iter()
                    .map(|(key, template)| {
                        (
                            key.clone(),
                            TemplatePreview::new(
                                &self.messages_tx,
                                template.clone(),
                                Some(props.profile.id.clone()),
                            ),
                        )
                    })
                    .collect_vec()
            });

        let table = Table {
            header: Some(["Field", "Value"]),
            rows: fields
                .iter()
                .map(|(key, value)| [key.as_str().into(), value.generate()])
                .collect_vec(),
            alternate_row_style: true,
            ..Default::default()
        };
        frame.render_widget(table.generate(), area);
    }
}

impl Persistable for ProfileId {
    type Persisted = Self;

    fn get_persistent(&self) -> &Self::Persisted {
        self
    }
}

/// Needed for preselection
impl PartialEq<Profile> for ProfileId {
    fn eq(&self, other: &Profile) -> bool {
        self == &other.id
    }
}