---
title: Discord commands and ratings
description: The four slash commands, how to phrase a question, what the buttons do, and what a rating changes.
sidebar:
  order: 1
---

## `/judge question:`

Ask in plain language. Nicknames work ("bob", "goyf", "snappy", "t3feri"). The alias list
is `data/aliases.yaml` in the repository, and pull requests adding to it are welcome. Use
brackets like `[[Full Card Name]]` to avoid ambiguity. A bracketed name matches only the
card with that exact name (current or printed, including one face of a split or
double-faced card). Anything else in brackets, such as a nickname or a near miss, is
offered back as a choice rather than corrected. Answers take twenty to forty-five seconds.
The bot acknowledges at once and edits the reply in.

The reply opens with a non-pinging `@you asked:` header, then the ruling, then a citation
per line. Rule citations link to the rule on the Yawgatog mirror of the Comprehensive
Rules. Rulings and Oracle text link to the card on Scryfall. The footer names the cards
the question was resolved to, which lets you check that "bob" was taken to mean Dark
Confidant. It then gives the model's confidence and the CR version it answered from.

If a name could mean several cards ("Tibalt", "Emrakul") you get a **did you mean…?** row
of up to five buttons instead of a guess. Only the person who asked can pick. If a name
matches nothing, the reply says which and suggests `[[Full Card Name]]`. Tournament-policy and
price questions are declined after the cheap classification step, before the expensive
synthesis call.

Ask a follow-up in the same channel and the bot sees the recent question-and-answer pairs
from that channel as history, so "what if it had flash?" works.

## Rating buttons

Every answer has three buttons: **Incorrect**, **Partially correct**, **Correct**. Rating
again replaces your earlier rating. A rating changes one thing: which past answers are
shown to the model as *examples* when a similar question comes in. Scores are smoothed (a
Bayesian mean with a prior of "partially correct" and a weight of three votes), so one
early vote cannot swing an answer's standing. Answers rated below 1.5 with at least five
votes are excluded.

Members holding the server's judge role (`JUDGE_ROLE`, default `Judge`) rate with an
override. The most recent judge rating replaces the crowd's score for that answer. The
rules always outrank examples. Prior answers are rendered after the CR material and labeled
with their rating, and the model is told they are precedent, not authority.

A past answer is retired automatically when its citations stop holding against the
current rules, rulings or Oracle text (a new CR release, an erratum). It comes back if
the text is restored.

## `/help`

What the bot does, how to ask, what it stores, and where the source is (the same notice
as `/license`). The reply is ephemeral, so only you see it.

## `/license`

The source offer: the repository holding this instance's source code, the commit it was
built from (linked into the repository), the licence (AGPL-3.0-or-later) and the
copyright. An operator running a modified version points it at their fork with
`JUDGE_SOURCE_URL`. An unmodified build names the upstream repository. The reply is
ephemeral.

## `/forget`

Deletes every rating you have recorded and tells you how many there were. Ratings are the
only data tied to your user id. Questions are stored against the channel, not the asker.
