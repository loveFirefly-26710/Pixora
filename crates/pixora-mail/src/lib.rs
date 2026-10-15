//! Pixora 的 SMTP 客户端与邮件模板。
//!
//! 两件事：把一封信真的发出去（[`Mailer`]），以及把模板里的占位符填上
//! （[`template`]）。重试与退避不在这里——那是发件箱与队列的活。

pub mod smtp;
pub mod template;

pub use smtp::{MailConfig, MailError, MailSecurity, Mailer};
pub use template::{placeholders, render, render_subject, spec, url_encode, MailVars, CATALOG};
