---
title: Use the hosted instance
description: The public web page and Discord bot run by the project, what they store, and how the ratings work.
sidebar:
  order: 1
---

The maintainer runs a public instance at **<https://mtgjudge.rpeters.dev>**. Type a rules
question and get a ruling with its citations; write a card as `[[Full Card Name]]` to pin
it when a nickname could mean several things. There is no login. The page is rate limited
per IP address (a handful of questions every few minutes) because each answer costs the
operator real money in model calls, and you cannot rate answers there.

## The Discord bot

The same instance can join Discord servers, where answers gain **rating buttons**
(incorrect / partially correct / correct) that shape which past answers are shown as
examples later. If you want it in your server, open an issue on the repository or
contact the maintainer; the bot is added to servers by invitation while the per-server
controls described in the [tenancy proposal](../../design-history/proposal-tenancy/)
are not yet built. You can also [run your own](../../self-hosting/first-run/).

Once it is in a server, `/help` explains the commands; the
[Discord commands](../../using/discord/) page has the details.

## What is stored

For every question answered: the question text, the answer, the channel or web session it
was asked in, and the ids of the rules, rulings and cards it was answered from. When you
press a rating button, your Discord user id and the score. Nothing else: the bot receives
only its own slash commands and button presses, never channel messages. `/forget` deletes
your ratings, which is the only data tied to you.

## Answers are AI-generated

Every citation is checked against its source before it is shown, which rules out invented
rule numbers and misquoted text. It does not rule out a wrong conclusion drawn from correct
quotes. Verify anything that matters at a tournament with a human judge. The bot declines
tournament-policy questions (the Magic Tournament Rules and Infraction Procedure Guide)
rather than answering them badly.
