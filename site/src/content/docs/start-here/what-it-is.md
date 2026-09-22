---
title: What the judge is
description: What a judgebot does, why each community runs its own, what an instance stores, and the limits of an AI answer.
sidebar:
  order: 1
---

A judgebot answers Magic: The Gathering rules questions the way a judge would. It gives a
short ruling. Every claim in it is backed by a quote from the Comprehensive Rules, an
official Scryfall ruling or the card's current Oracle text. The quote is checked against
its source before the answer is shown, so a rule number the model invented never reaches
you. Ask from Discord with `/judge`, from a web page, or from the command line. Use
brackets like `[[Full Card Name]]` when a nickname could mean several things.

## One bot per community

Every judgebot is its own Discord application, run by the community that uses it. Each
has its own spend cap, judge role and bot token. The person who chose the model pays for
the questions, and nobody shares a budget with strangers. The
[design decisions](../../how-it-works/decisions/#d16-one-judgebot-per-community)
page has the reasoning.

Setting one up takes a compose file, a model API key, and a Discord application you
create in the developer portal in a few minutes:

1. [Requirements and first run](../../self-hosting/first-run/): the database, the data
   loads, and the web page on your own machine.
2. [Create the Discord app](../../self-hosting/discord-app/): the portal steps, with links
   into Discord's documentation for each.
3. [Production deployment](../../self-hosting/deployment/), when it should stay up without
   your laptop.

You can see the judge work before you have a bot token:
[Try it without Discord](../without-discord/) brings up the web page and the command
line. Once your bot is in a server, `/help` explains the commands, and the
[Discord commands](../../using/discord/) page has the details.

## What an instance stores

For every question answered, an instance stores:

- the question text and the answer
- the channel or web session it was asked in
- the ids of the rules, rulings and cards it was answered from.

A Discord question asked with `private: True` is not saved. When someone presses a rating
button, the instance stores their Discord user id and the score. It stores nothing else.
The bot receives only its own slash commands and button presses, never channel messages.
Ratings are the only data tied to a user, and `/forget` deletes them.

[Sample answers](../../start-here/sample-answers/) shows what comes back, and
[Evaluation](../../how-it-works/evaluation/#results) how it scored on a hard question set.

## AI-generated answers

Every citation is checked against its source before it is shown. Every rule number named
in the answer's text must also be cited. That rules out invented rule numbers and
misquoted text. It does not rule out a wrong conclusion drawn from correct quotes. Verify anything that matters at a tournament with a human judge. The bot declines
tournament-policy questions (the Magic Tournament Rules and Infraction Procedure Guide)
rather than answering them badly.
