# Character reference and continuity outline

1. Establish consent/rights for the character and source images. Write an approved description of appearance, clothing, proportions, palette, distinguishing details, and forbidden changes.
2. Gather clear local reference images and record provenance. Separate approved references from experiments; preserve originals.
3. Create a reference sheet with labeled views and detail crops. Native Takumi can lay out existing images and text; it does not invent new views of a character.
4. Save reusable creative direction with `skywrite styles create character-name --file character-style.md`. Save project-specific procedural instructions as a personal skill when needed.
5. Before any generative image/video request, check current CLI help and model reference-image support. Choose the approved reference deliberately, set a budget, and retain returned job IDs and provenance when the tool provides them.
6. Review each candidate for face, clothing, markings, scale, hands, and cross-shot continuity. Reject identity drift; an attractive output alone does not establish fidelity.
7. Assemble only approved assets in the edit, then validate and review the rendered sequence. Use upscaling only when an available backend explicitly supports that task; a generative higher-resolution edit is not automatically faithful upscaling.

This is a workflow outline. It makes no claim that all generation or upscaling backends are installed. Never automatically execute saved prompts or skills.

```sh
skywrite generate image --model MODEL --prompt 'Original character design' --output character.png
skywrite generate image --model MODEL --reference character.png --prompt 'Same character, another pose' --output pose.png
skywrite generate video --model MODEL --first-frame character.png --prompt 'Animate this character'
skywrite jobs resume JOB_ID --wait 60 --output shot.mp4
skywrite upscale video --source-job JOB_ID --model UPSCALE_MODEL --factor 2
```

Discover supported parameters first. H3 Max accepts only one keyframe per request. Inspect character consistency visually. Resume existing jobs rather than automatically retrying an uncertain paid submit. Source-job upscale uses fresh signed URLs with stable content identity.

## Keep identity references separate from shot design

A reference showing an entire garden can anchor the garden layout, framing, and camera position as strongly as it anchors the character. In the Pip production, repeatedly conditioning on the same full garden image encouraged a locked camera and similar compositions. Treat that as a planning lesson, not a guarantee about a provider's behavior.

Use a character-focused identity reference when identity is the goal. Keep environment/layout references separate when the model supports multiple explicit roles; with only one supported image, choose the reference whose composition helps the intended shot and describe the remaining identity/environment constraints explicitly. A full scene reference is appropriate when layout continuity matters.

Plan shot purpose, subject scale, camera angle, viewpoint, and camera movement before generating. Vary these deliberately across shots when the story calls for it: an establishing view, an insert of the story object, a character reaction, and a view from another position serve different purposes. Review an adjacent-shot contact sheet for repeated subject scale and framing before committing to an edit. Repeated framing can be intentional; a request does not guarantee motion or a particular composition. Retakes require explicit authorization and must never be submitted automatically to repair variety.
