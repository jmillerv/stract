<script lang="ts">
  import type { StackOverflowQuestion, StackOverflowAnswer } from '$lib/api';
  import Code from '$lib/components/Code.svelte';
  import HandThumbUp from '~icons/heroicons/hand-thumb-up';
  import Check from '~icons/heroicons/check';
  import StackOverflowText from './StackOverflowText.svelte';

  export let question: StackOverflowQuestion;
  export let answers: StackOverflowAnswer[];
</script>

<div class="line-clamp-2">
  {#each question.body as passage}
    {#if passage.type == 'text'}
      {passage.value}
    {/if}
  {/each}
</div>
<div class="flex space-x-4 pt-2 text-xs">
  {#each answers.slice(0, 3) as answer}
    <div class="w-1/3 overflow-hidden">
      <a
        class="block h-56 overflow-hidden rounded-lg border p-2 hover:bg-base-200/80"
        href={answer.url}
      >
        <div class="mb-1 flex w-full items-center justify-between space-x-1 text-xs text-neutral">
          <div class="flex">
            {answer.date}
          </div>
          <div class="flex space-x-1">
            <span class="h-fit">
              {answer.upvotes}
            </span>
            <div class="h-fit">
              <HandThumbUp class="w-4" />
            </div>
            {#if answer.accepted}
              <div class="h-fit text-green-600">
                <Check class="w-4" />
              </div>
            {/if}
          </div>
        </div>
        <div>
          {#each answer.body as passage}
            {#if passage.type == 'text'}
              <StackOverflowText text={passage.value} />
            {:else if passage.type == 'code'}
              <Code code={passage.value} transparentBackground={true} />
            {/if}
          {/each}
        </div>
      </a>
    </div>
  {/each}
</div>
