//! The emails Parlour sends about accounts, rendered from the plain-text
//! templates in `templates/email/`.

use askama::Template;

use crate::mail::Email;

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
}
