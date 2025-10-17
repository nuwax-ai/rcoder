import { useState, useCallback, useEffect, useRef } from 'react';
import { Cell, GameConfig, GameState, GameStats, CellState } from '../types/minesweeper';
import {
  createBoard,
  placeMines,
  calculateAdjacentMines,
  revealCell,
  toggleFlag,
  getGameState,
  revealAllMines,
} from '../utils/minesweeper';

export const useMinesweeper = (config: GameConfig) => {
  const [board, setBoard] = useState<Cell[][]>(() => createBoard(config));
  const [gameState, setGameState] = useState<GameState>(GameState.READY);
  const [gameStats, setGameStats] = useState<GameStats>({
    timeElapsed: 0,
    flagsUsed: 0,
    cellsRevealed: 0,
  });
  const [firstClick, setFirstClick] = useState(true);

  const timerRef = useRef<NodeJS.Timeout | null>(null);

  // 计时器逻辑
  useEffect(() => {
    if (gameState === GameState.PLAYING) {
      timerRef.current = setInterval(() => {
        setGameStats(prev => ({ ...prev, timeElapsed: prev.timeElapsed + 1 }));
      }, 1000);
    } else {
      if (timerRef.current) {
        clearInterval(timerRef.current);
        timerRef.current = null;
      }
    }

    return () => {
      if (timerRef.current) {
        clearInterval(timerRef.current);
      }
    };
  }, [gameState]);

  // 开始新游戏
  const startNewGame = useCallback(() => {
    setBoard(createBoard(config));
    setGameState(GameState.READY);
    setGameStats({
      timeElapsed: 0,
      flagsUsed: 0,
      cellsRevealed: 0,
    });
    setFirstClick(true);
    if (timerRef.current) {
      clearInterval(timerRef.current);
      timerRef.current = null;
    }
  }, [config]);

  // 处理格子点击
  const handleCellClick = useCallback((row: number, col: number) => {
    if (gameState === GameState.WON || gameState === GameState.LOST) {
      return;
    }

    const cell = board[row][col];

    // 不能点击已标记的格子
    if (cell.state === CellState.FLAGGED) {
      return;
    }

    let newBoard = [...board.map(row => [...row])];

    // 第一次点击时放置地雷
    if (firstClick) {
      placeMines(newBoard, config, row, col);
      calculateAdjacentMines(newBoard);
      setFirstClick(false);
      setGameState(GameState.PLAYING);
    }

    // 揭露格子
    newBoard = revealCell(newBoard, row, col);

    // 检查游戏状态
    const newGameState = getGameState(newBoard, false);

    if (newGameState === GameState.LOST) {
      revealAllMines(newBoard);
    }

    // 更新统计信息
    let flagsUsed = 0;
    let cellsRevealed = 0;

    for (const row of newBoard) {
      for (const cell of row) {
        if (cell.state === CellState.FLAGGED) {
          flagsUsed++;
        }
        if (cell.state === CellState.REVEALED) {
          cellsRevealed++;
        }
      }
    }

    setBoard(newBoard);
    setGameState(newGameState);
    setGameStats(prev => ({
      ...prev,
      flagsUsed,
      cellsRevealed,
    }));
  }, [board, firstClick, gameState, config]);

  // 处理右键点击（标记）
  const handleRightClick = useCallback((row: number, col: number) => {
    if (gameState === GameState.WON || gameState === GameState.LOST) {
      return;
    }

    if (firstClick) {
      return; // 第一次点击不能标记
    }

    const cell = board[row][col];

    if (cell.state === CellState.REVEALED) {
      return; // 已揭露的格子不能标记
    }

    const newBoard = toggleFlag([...board.map(row => [...row])], row, col);

    // 更新标记数量
    let flagsUsed = 0;
    for (const row of newBoard) {
      for (const cell of row) {
        if (cell.state === CellState.FLAGGED) {
          flagsUsed++;
        }
      }
    }

    setBoard(newBoard);
    setGameStats(prev => ({ ...prev, flagsUsed }));
  }, [board, gameState, firstClick]);

  // 双击揭露相邻格子（当所有相邻地雷都被标记时）
  const handleDoubleClick = useCallback((row: number, col: number) => {
    if (gameState !== GameState.PLAYING) {
      return;
    }

    const cell = board[row][col];

    if (cell.state !== CellState.REVEALED || cell.adjacentMines === 0) {
      return;
    }

    // 计算周围已标记的地雷数量
    let flaggedCount = 0;
    const neighbors: Array<[number, number]> = [];

    for (let dr = -1; dr <= 1; dr++) {
      for (let dc = -1; dc <= 1; dc++) {
        if (dr === 0 && dc === 0) continue;

        const newRow = row + dr;
        const newCol = col + dc;

        if (newRow >= 0 && newRow < config.rows && newCol >= 0 && newCol < config.cols) {
          neighbors.push([newRow, newCol]);
          if (board[newRow][newCol].state === CellState.FLAGGED) {
            flaggedCount++;
          }
        }
      }
    }

    // 如果标记数量等于相邻地雷数量，揭露所有未标记的相邻格子
    if (flaggedCount === cell.adjacentMines) {
      let newBoard = [...board.map(row => [...row])];

      for (const [nRow, nCol] of neighbors) {
        if (newBoard[nRow][nCol].state === CellState.HIDDEN) {
          newBoard = revealCell(newBoard, nRow, nCol);
        }
      }

      const newGameState = getGameState(newBoard, false);

      if (newGameState === GameState.LOST) {
        revealAllMines(newBoard);
      }

      // 更新统计信息
      let flagsUsed = 0;
      let cellsRevealed = 0;

      for (const row of newBoard) {
        for (const cell of row) {
          if (cell.state === CellState.FLAGGED) {
            flagsUsed++;
          }
          if (cell.state === CellState.REVEALED) {
            cellsRevealed++;
          }
        }
      }

      setBoard(newBoard);
      setGameState(newGameState);
      setGameStats(prev => ({
        ...prev,
        flagsUsed,
        cellsRevealed,
      }));
    }
  }, [board, gameState, config]);

  return {
    board,
    gameState,
    gameStats,
    handleCellClick,
    handleRightClick,
    handleDoubleClick,
    startNewGame,
  };
};