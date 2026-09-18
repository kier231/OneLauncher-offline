use freya::prelude::*;
use freya::query::MutationStateData;
use oneclient_auth::MinecraftAccount;

use crate::components::{Avatar, Button, Icon, IconType, TextInput};
use crate::hooks::{
    AddOfflineAccountKeys, try_default_account, use_add_offline_account, use_current_account,
};
use crate::routes::Route;
use crate::theme::colors;
use crate::view::onboarding::{
    onboarding_illustration, onboarding_nav, onboarding_page, step_heading,
};

#[derive(PartialEq)]
pub struct OnboardingAccount;

impl Component for OnboardingAccount {
    fn render(&self) -> impl IntoElement {
        let account_query = use_current_account();
        let add_offline = use_add_offline_account();
        let username = use_state(String::new);

        let account = try_default_account(&account_query);
        let has_account = account.is_some();
        let pending = add_offline.read().state().is_loading();
        let error = match &*add_offline.read().state() {
            MutationStateData::Settled { res: Err(err), .. } => Some(err.to_string()),
            MutationStateData::Loading {
                res: Some(Err(err)),
            } => Some(err.to_string()),
            _ => None,
        };

        let add_account = move |_| {
            let name = username.peek().trim().to_string();
            if !name.is_empty() {
                add_offline.mutate(AddOfflineAccountKeys { username: name });
            }
        };

        let content = rect()
            .vertical()
            .width(Size::fill())
            .spacing(24.)
            .child(step_heading(
                "Account",
                "Choose the offline Minecraft username you want to use.",
            ))
            .child(match &account {
                Some(account) => account_preview(account).into_element(),
                None => offline_account_form(username, pending, error, add_account).into_element(),
            })
            .into_element();

        let page = onboarding_page(
            onboarding_illustration(IconType::OnboardingAccount),
            content,
            onboarding_nav(
                Some(Route::OnboardingLanguage {}),
                Route::OnboardingBundles {},
                has_account,
            ),
        );

        rect().width(Size::fill()).height(Size::fill()).child(page)
    }
}

fn account_preview(account: &MinecraftAccount) -> impl IntoElement {
    rect()
        .horizontal()
        .width(Size::fill())
        .spacing(24.)
        .child(
            rect()
                .horizontal()
                .spacing(12.)
                .cross_align(Alignment::Center)
                .child(
                    Avatar::new(account.id.to_string())
                        .width(Size::px(48.))
                        .height(Size::px(48.)),
                )
                .child(
                    rect()
                        .vertical()
                        .spacing(4.)
                        .child(
                            label()
                                .text(account.username.clone())
                                .font_size(16.)
                                .font_weight(FontWeight::SEMI_BOLD)
                                .color(colors::fg_primary()),
                        )
                        .child(
                            label()
                                .text(account.id.to_string())
                                .font_size(12.)
                                .color(colors::fg_secondary()),
                        ),
                ),
        )
        .into_element()
}

fn offline_account_form(
    username: State<String>,
    pending: bool,
    error: Option<String>,
    on_add: impl FnMut(Event<PressEventData>) + 'static,
) -> impl IntoElement {
    rect()
        .vertical()
        .width(Size::fill())
        .spacing(12.)
        .cross_align(Alignment::Start)
        .child(TextInput::new(username).placeholder("Offline username"))
        .child(
            Button::new()
                .primary()
                .large()
                .enabled(!pending)
                .on_press(on_add)
                .child(Icon::new(IconType::Plus).size(16.))
                .text(if pending {
                    "Adding account..."
                } else {
                    "Add offline account"
                }),
        )
        .maybe_child(error.map(|message| {
            rect()
                .horizontal()
                .cross_align(Alignment::Center)
                .spacing(6.)
                .child(
                    Icon::new(IconType::AlertTriangle)
                        .size(13.)
                        .color(colors::danger()),
                )
                .child(label().text(message).font_size(12.).color(colors::danger()))
                .into_element()
        }))
        .into_element()
}
