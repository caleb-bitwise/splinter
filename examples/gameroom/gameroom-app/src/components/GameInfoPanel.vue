<!--
Copyright 2018-2022 Cargill Incorporated

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
-->

<template>
  <div class="game-info-panel">
    <div class="game-info-container">
      <div class="header">
        <div class="game-icon">
          XO
        </div>
        <div class="info">
          <div>
            Created: {{ fromNow(gameInfo.createdTimestamp) }}
          </div>
          <div v-if="gameInfo.playerOne">
            Last move: {{ fromNow(gameInfo.lastTurnTimestamp) }}
          </div>
        </div>
      </div>
      <div class="players">
        <div class="player" :class="{active: playerOneActive}">
          <i class="icon material-icons-round">{{ playerOneIcon }}</i>
          {{ gameInfo.playerOne.substring(0,6) }}
        </div>
        <div class="player" :class="{active: playerTwoActive}">
          <i class="icon material-icons-round">{{ playerTwoIcon }}</i>
          {{ gameInfo.playerTwo.substring(0,6) }}
        </div>
      </div>
      <div class="status">
        {{ status }}
      </div>
    </div>
  </div>
</template>

<script lang="ts">
import { Vue, Component, Prop } from 'vue-property-decorator';
import * as moment from 'moment';

export enum GameStatus {
  PlayerOneNext,
  PlayerTwoNext,
  PlayerOneWin,
  PlayerTwoWin,
  Tie,
}

export interface GameInfo {
  gameType: string;
  playerOne: string;
  playerTwo: string;
  status: GameStatus;
  createdTimestamp: number;
  lastTurnTimestamp: number;
}

@Component
export default class GameInfoPanel extends Vue {
  @Prop() gameInfo!: GameInfo;

  get status(): string {
    const publicKey = this.$store.getters['user/getPublicKey'];
    if (!this.gameInfo.playerOne) {
      return 'Take a space to join the game as X';
    } else if (!this.gameInfo.playerTwo) {
      if (publicKey === this.gameInfo.playerOne) {
        return 'Waiting for another player';
      }
      return 'Take a space to join the game as O';
    }


    switch (this.gameInfo.status) {
      case(GameStatus.PlayerOneNext):
        if (publicKey === this.gameInfo.playerOne) {
          return 'Your turn';
        } else {
          return `${this.gameInfo.playerOne.substring(0, 6)}'s turn`;
        }
      case(GameStatus.PlayerTwoNext):
        if (publicKey === this.gameInfo.playerTwo) {
          return 'Your turn';
        } else {
          return `${this.gameInfo.playerTwo.substring(0, 6)}'s turn`;
        }
      case(GameStatus.PlayerOneWin):
        if (publicKey === this.gameInfo.playerOne) {
          return 'You won';
        } else {
          return `${this.gameInfo.playerOne.substring(0, 6)} won`;
        }
      case(GameStatus.PlayerTwoWin):
        if (publicKey === this.gameInfo.playerTwo) {
          return 'You won';
        } else {
          return `${this.gameInfo.playerTwo.substring(0, 6)} won`;
        }
      case(GameStatus.Tie): return 'Game resulted in a draw';
    }
  }

  get playerOneActive(): boolean {
    return [GameStatus.PlayerOneNext, GameStatus.PlayerOneWin].includes(this.gameInfo.status);
  }

  get playerTwoActive(): boolean {
    return [GameStatus.PlayerTwoNext, GameStatus.PlayerTwoWin].includes(this.gameInfo.status);
  }

  get playerOneIcon(): string {
    if (this.playerOneActive) {
      return 'person';
    } else { return 'person_outline'; }
  }

  get playerTwoIcon(): string {
    if (this.playerTwoActive) {
      return 'person';
    } else { return 'person_outline'; }
  }

  fromNow(timestamp: number): string {
    return moment.unix(timestamp).fromNow();
  }
}

</script>

<style lang="scss">
  @import '@/scss/components/_game-info-panel.scss';
</style>
