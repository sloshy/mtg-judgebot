---
title: The maintainer's instance
description: What the public web page is, why the Discord bot is not offered for invitation, and how to get one of your own.
sidebar:
  order: 1
---

The maintainer runs an instance for their own Discord servers, and its web page is public
at **<https://mtgjudge.rpeters.dev>**. Type a rules question and get a ruling with its
citations; write a card as `[[Full Card Name]]` to pin it when a nickname could mean
several things. There is no login. The page is rate limited per IP address (a handful of
questions every few minutes) because each answer costs the operator real money in model
calls, and you cannot rate answers there. Treat it as a demonstration of the pipeline, not
a service with an uptime promise.

## Want it in your Discord server? Run your own

The bot is **not** offered for invitation. Every judgebot is meant to be its own Discord
application, run by the community that uses it: one process is one spend cap, one judge
role and one bot token, so the person who chose the model pays for the questions and
nobody shares a budget with strangers. The
[design decisions](../../how-it-works/decisions/#d16-one-judgebot-per-community-no-multi-tenancy)
page has the reasoning.

Setting one up is one compose file, a Discord application you create in the developer
portal in a few minutes, and a model API key:

1. [Requirements and first run](../../self-hosting/first-run/): the database, the data
   loads, and the web page on your own machine.
2. [Create the Discord app](../../self-hosting/discord-app/): the portal steps, with links
   into Discord's documentation for each.
3. [Production deployment](../../self-hosting/deployment/), when it should stay up without
   your laptop.

Once yours is in a server, `/help` explains the commands; the
[Discord commands](../../using/discord/) page has the details.

## What an instance stores

For every question answered: the question text, the answer, the channel or web session it
was asked in, and the ids of the rules, rulings and cards it was answered from. When
someone presses a rating button, their Discord user id and the score. Nothing else: the
bot receives only its own slash commands and button presses, never channel messages.
`/forget` deletes a user's ratings, which is the only data tied to them.

## Answers are AI-generated

Every citation is checked against its source before it is shown, which rules out invented
rule numbers and misquoted text. It does not rule out a wrong conclusion drawn from correct
quotes. Verify anything that matters at a tournament with a human judge. The bot declines
tournament-policy questions (the Magic Tournament Rules and Infraction Procedure Guide)
rather than answering them badly.
