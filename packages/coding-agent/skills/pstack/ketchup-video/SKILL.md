---
name: ketchup-video
description: "Catch the user up like /skill:ketchup, with a short silent video of slides for what happened and the waiting calls as text under it. Use for /skill:ketchup-video."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/ketchup-video
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Ketchup video

Follow `../ketchup/SKILL.md`, but show what happened in a short silent video instead of its summary and visuals. Its window starts at the last message the user wrote before `/skill:ketchup-video`, not before any earlier `/skill:ketchup`. Writing and rendering the video, in a scratch folder outside the repo, is the one exception to its read-only rule. The terminal cannot play a video, so the reply is ketchup's opening sentence, the video's absolute path, one line naming any work left that you can do yourself, and the action items. When the video shows a call's options, call each option by the name the video gives it. If the render fails or `ffmpeg` is missing, say so in one line and give only the calls.

## Slides

Use the screenshots, recordings, diffs, and data the work produced, not the session's prose about them. Before you draw, write the one sentence the user should leave with and one line per slide. Drop each slide that does not change what the user knows. Use at most 10 slides, and fewer is better. Neither note goes in the reply.

Each slide makes one point. Build the slides on one picture and change it from slide to slide, rather than cut to an unrelated one. A color or shape means the same thing on every slide. Show the work in pictures, such as how a system works, a PR stack with each PR's state, a before and after of a fix, or options side by side. Point at what changed with arrows, highlights, and motion. A slide is never a bullet list, a status list, a table, or boxes with nothing connecting them.

Keep text to the minimum. Put each label on the thing it names, in plain words a person would say, not tags like "Tests: pass". Never caption what a picture shows, and never give a fact twice, across slides or in the text under them.

Show what came of each thing the user asked for, and whether it is done, still running, failed, or not checked. Never show something done that still waits on the user. Show any bug found and fixed on the way. If a visible change has no screenshot, say so on its slide and name what changed on screen. Draw only from facts you have. Never invent a number, and never draw a screen you did not capture.

## Render

Make each slide a PNG of one landscape size, such as 1920x1080. Draw diagrams with `await diagram.mermaid(text, show=False, path=f"{out}/slide-{n:02}")` and composite them, with screenshots and labels, onto the slide with PIL. Render one silent landscape mp4 with `ffmpeg` through `bash`, with motion between slides, such as an `xfade` crossfade. Every frame is the slide itself, edge to edge. Make the frames from the slides, never by recording a screen. Hold each slide long enough to read its words, at least one second for each three words it adds. Words inside a screenshot do not count.

Before you give the path, look at one frame from each slide in the rendered file: extract it at the middle of the slide's hold (`ffmpeg -ss <t> -i video.mp4 -frames:v 1 frame-<n>.png`) and open it with `await view_image(path)`. Count the words it adds against its hold in that file. Fix any slide you cannot read in that time, render again, and check again.
