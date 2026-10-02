//! The emails Parlour sends about accounts, rendered from the plain-text
//! templates in `templates/email/`.

use askama::Template;

use crate::mail::Email;
use crate::notify::EmailItem;

#[derive(Template)]
#[template(path = "email/invite.txt")]
struct InviteText<'a> {
    site_name: &'a str,
    inviter: &'a str,
    url: &'a str,
    expires: &'a str,
}

pub fn invite(to: &str, site_name: &str, inviter: &str, url: &str, expires: &str) -> Email {
    Email {
        to: to.to_owned(),
        subject: format!("{inviter} has invited you to {site_name}"),
        body: InviteText {
            site_name,
            inviter,
            url,
            expires,
        }
        .render()
        .expect("email templates render"),
        secret: true,
        private: false,
        unsubscribe: None,
    }
}

#[derive(Template)]
#[template(path = "email/verify.txt")]
struct VerifyText<'a> {
    site_name: &'a str,
    name: &'a str,
    url: &'a str,
}

pub fn verify(to: &str, site_name: &str, name: &str, url: &str) -> Email {
    Email {
        to: to.to_owned(),
        subject: format!("Confirm your email address for {site_name}"),
        body: VerifyText {
            site_name,
            name,
            url,
        }
        .render()
        .expect("email templates render"),
        secret: true,
        private: false,
        unsubscribe: None,
    }
}

#[derive(Template)]
#[template(path = "email/magic_link.txt")]
struct MagicLinkText<'a> {
    site_name: &'a str,
    name: &'a str,
    url: &'a str,
    code: &'a str,
}

pub fn magic_link(to: &str, site_name: &str, name: &str, url: &str, code: &str) -> Email {
    Email {
        to: to.to_owned(),
        subject: format!("Your login link for {site_name}"),
        body: MagicLinkText {
            site_name,
            name,
            url,
            code,
        }
        .render()
        .expect("email templates render"),
        secret: true,
        private: false,
        unsubscribe: None,
    }
}

#[derive(Template)]
#[template(path = "email/reset.txt")]
struct ResetText<'a> {
    site_name: &'a str,
    name: &'a str,
    url: &'a str,
}

pub fn reset(to: &str, site_name: &str, name: &str, url: &str) -> Email {
    Email {
        to: to.to_owned(),
        subject: format!("Reset your password for {site_name}"),
        body: ResetText {
            site_name,
            name,
            url,
        }
        .render()
        .expect("email templates render"),
        secret: true,
        private: false,
        unsubscribe: None,
    }
}

#[derive(Template)]
#[template(path = "email/join.txt")]
struct JoinText<'a> {
    site_name: &'a str,
    name: &'a str,
    url: &'a str,
}

/// The link that finishes joining with an invite link.
pub fn join(to: &str, site_name: &str, name: &str, url: &str) -> Email {
    Email {
        to: to.to_owned(),
        subject: format!("Finish joining {site_name}"),
        body: JoinText {
            site_name,
            name,
            url,
        }
        .render()
        .expect("email templates render"),
        secret: true,
        private: false,
        unsubscribe: None,
    }
}

#[derive(Template)]
#[template(path = "email/already_member.txt")]
struct AlreadyMemberText<'a> {
    site_name: &'a str,
    name: &'a str,
    what: &'a str,
    username: &'a str,
    login_url: &'a str,
    reset_url: &'a str,
}

/// Why a member is being told they already have an account.
pub enum AlreadyMember<'a> {
    /// This member invited the address.
    InvitedBy(&'a str),
    /// Someone used an invite link with the address.
    Joining,
}

/// Sent instead of an invitation, or instead of the link to finish joining,
/// when the address belongs to a member already: the page that asked says
/// the same either way, so it doesn't reveal who is a member.
pub fn already_member(
    to: &str,
    site_name: &str,
    name: &str,
    username: &str,
    why: AlreadyMember,
    login_url: &str,
    reset_url: &str,
) -> Email {
    let what = match why {
        AlreadyMember::InvitedBy(inviter) => format!("{inviter} invited you to join {site_name}"),
        AlreadyMember::Joining => {
            format!("Someone used an invitation to join {site_name} with this email address")
        }
    };
    Email {
        to: to.to_owned(),
        subject: format!("You already have an account on {site_name}"),
        body: AlreadyMemberText {
            site_name,
            name,
            what: &what,
            username,
            login_url,
            reset_url,
        }
        .render()
        .expect("email templates render"),
        secret: false,
        private: false,
        unsubscribe: None,
    }
}

#[derive(Template)]
#[template(path = "email/test.txt")]
struct TestText<'a> {
    site_name: &'a str,
}

pub fn test(to: &str, site_name: &str) -> Email {
    Email {
        to: to.to_owned(),
        subject: format!("Test email from {site_name}"),
        body: TestText { site_name }
            .render()
            .expect("email templates render"),
        secret: false,
        private: false,
        unsubscribe: None,
    }
}

#[derive(Template)]
#[template(path = "email/notifications.txt")]
struct NotificationsText<'a> {
    site_name: &'a str,
    name: &'a str,
    items: &'a [EmailItem],
    settings_url: &'a str,
    unsubscribe_url: &'a str,
    unsubscribe_what: &'a str,
}

/// Notifications a member hasn't seen, with one-click unsubscribe headers
/// (RFC 8058) for `unsubscribe_what`.
pub fn notifications(
    to: &str,
    site_name: &str,
    name: &str,
    items: &[EmailItem],
    settings_url: &str,
    unsubscribe_url: &str,
    unsubscribe_what: &str,
) -> Email {
    let subject = match items {
        [one] => format!("{} · {site_name}", one.headline),
        _ => format!("{} new notifications on {site_name}", items.len()),
    };
    Email {
        to: to.to_owned(),
        subject,
        body: NotificationsText {
            site_name,
            name,
            items,
            settings_url,
            unsubscribe_url,
            unsubscribe_what,
        }
        .render()
        .expect("email templates render"),
        secret: false,
        private: true,
        unsubscribe: Some(unsubscribe_url.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_plain_text() {
        let email = invite(
            "alex@example.com",
            "The <Tuesday> Club",
            "Ruth & co",
            "https://forum.example.com/invite/abc",
            "8 Oct 2026",
        );
        assert!(
            email
                .body
                .contains("Ruth & co has invited you to join The <Tuesday> Club.")
        );
        assert!(email.body.contains("https://forum.example.com/invite/abc"));
        let email = magic_link(
            "r@example.com",
            "Club",
            "Ruth",
            "https://x/login/t",
            "012345",
        );
        assert!(!email.subject.contains("012345"));
        assert!(email.body.contains("012345"));
    }

    /// Emails with links that work like passwords say so, so that the job
    /// queue doesn't keep them once they can't be sent.
    #[test]
    fn emails_with_live_links_are_secret() {
        let url = "https://x/y";
        for email in [
            invite("a@x", "Club", "Ruth", url, "today"),
            verify("a@x", "Club", "Ruth", url),
            magic_link("a@x", "Club", "Ruth", url, "012345"),
            reset("a@x", "Club", "Ruth", url),
            join("a@x", "Club", "Ruth", url),
        ] {
            assert!(email.secret, "{}", email.subject);
        }
        for email in [
            already_member(
                "a@x",
                "Club",
                "Ruth",
                "ruth",
                AlreadyMember::Joining,
                url,
                url,
            ),
            test("a@x", "Club"),
        ] {
            assert!(!email.secret, "{}", email.subject);
        }
    }
}
