// Candlestick + volume chart on lightweight-charts v5 (the same library the
// market-data system's page embeds).
//
// Bars arrive pre-aggregated from the feed provider (tape first, server kline
// frames layered on top), so this component only paints: create the chart once
// and call `setData` with the whole series on every flush. `setData` instead of
// `update` removes the out-of-order/`Cannot update oldest data` class of errors,
// and switching symbol or interval simply yields a new array.

import { useEffect, useRef, useState } from "react";
import {
  CandlestickSeries,
  ColorType,
  HistogramSeries,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type UTCTimestamp,
} from "lightweight-charts";
import { useMarketFeed } from "../hooks/useMarketFeed";

export function Chart() {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const candlesRef = useRef<ISeriesApi<"Candlestick"> | null>(null);
  const volumeRef = useRef<ISeriesApi<"Histogram"> | null>(null);
  const { interval, subscribeBars, historyCount, historyError } = useMarketFeed();
  const [hasBars, setHasBars] = useState(false);

  // ---- create the chart once ----
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const chart = createChart(container, {
      autoSize: true,
      layout: {
        background: { type: ColorType.Solid, color: "transparent" },
        textColor: "#868f9f",
        fontSize: 11,
        attributionLogo: true,
      },
      grid: {
        vertLines: { color: "rgba(42, 46, 55, 0.6)" },
        horzLines: { color: "rgba(42, 46, 55, 0.6)" },
      },
      rightPriceScale: { borderColor: "#2b3139" },
      timeScale: {
        borderColor: "#2b3139",
        timeVisible: true,
        secondsVisible: true,
        rightOffset: 6,
        barSpacing: 6,
      },
      crosshair: {
        mode: 0,
        vertLine: { color: "#5d6673", labelBackgroundColor: "#2b3139" },
        horzLine: { color: "#5d6673", labelBackgroundColor: "#2b3139" },
      },
    });

    const candles = chart.addSeries(CandlestickSeries, {
      upColor: "#2ebd85",
      downColor: "#f6465d",
      borderUpColor: "#2ebd85",
      borderDownColor: "#f6465d",
      wickUpColor: "#2ebd85",
      wickDownColor: "#f6465d",
      // The right-axis label carries the last price; the dashed line across
      // the plot would sit on top of a flat one-trade candle.
      priceLineVisible: false,
      lastValueVisible: true,
    });
    const volume = chart.addSeries(HistogramSeries, {
      priceFormat: { type: "volume" },
      priceScaleId: "",
      priceLineVisible: false,
      lastValueVisible: false,
    });
    volume.priceScale().applyOptions({ scaleMargins: { top: 0.82, bottom: 0 } });

    chartRef.current = chart;
    candlesRef.current = candles;
    volumeRef.current = volume;

    return () => {
      chartRef.current = null;
      candlesRef.current = null;
      volumeRef.current = null;
      chart.remove();
    };
  }, []);

  // ---- follow the selected interval ----
  useEffect(() => {
    const chart = chartRef.current;
    const candles = candlesRef.current;
    const volume = volumeRef.current;
    if (!chart || !candles || !volume) return;

    chart.timeScale().applyOptions({ secondsVisible: interval === "1s" });

    let lastWindow: number | null = null;
    let painted = false;

    const unsubscribe = subscribeBars(interval, (bars) => {
      candles.setData(
        bars.map((bar) => ({
          time: bar.time as UTCTimestamp,
          open: bar.open,
          high: bar.high,
          low: bar.low,
          close: bar.close,
        })),
      );
      volume.setData(
        bars.map((bar) => ({
          time: bar.time as UTCTimestamp,
          value: bar.volume,
          color: bar.close >= bar.open ? "rgba(46, 189, 133, 0.45)" : "rgba(246, 70, 93, 0.45)",
        })),
      );

      setHasBars(bars.length > 0);
      if (!bars.length) {
        painted = false;
        lastWindow = null;
        return;
      }
      // Bars are right-anchored like the reference terminal, so a fresh page
      // with two trades shows two candles at the right edge instead of two
      // giants stretched across the panel.
      const newest = bars[bars.length - 1].time;
      if (!painted || newest !== lastWindow) {
        chart.timeScale().scrollToRealTime();
        painted = true;
      }
      lastWindow = newest;
    });

    return unsubscribe;
  }, [interval, subscribeBars]);

  return (
    <div className="panel chart-panel">
      <div className="panel-head">
        <h2>
          Chart
          {historyCount > 0 ? (
            <span className="small muted"> · history {historyCount}</span>
          ) : historyError ? (
            <span className="small muted"> · {historyError}</span>
          ) : null}
        </h2>
        <IntervalToggle />
      </div>
      <div className="chart-wrap">
        <div ref={containerRef} className="chart-container" />
        {hasBars ? null : (
          <div className="chart-empty">
            <span className="muted">Waiting for candles…</span>
            <span className="small muted">
              History loads from TDengine; live fills update the open bar.
            </span>
          </div>
        )}
      </div>
    </div>
  );
}

function IntervalToggle() {
  const { interval, setInterval_ } = useMarketFeed();
  return (
    <div className="interval-toggle" role="tablist" aria-label="Kline interval">
      {(["1s", "1m"] as const).map((option) => (
        <button
          key={option}
          type="button"
          className={option === interval ? "active" : ""}
          onClick={() => setInterval_(option)}
        >
          {option}
        </button>
      ))}
    </div>
  );
}
